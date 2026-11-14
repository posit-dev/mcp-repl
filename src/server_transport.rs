use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rmcp::RoleServer;
use rmcp::model::{
    CallToolRequest, CallToolResponse, ClientNotification, ClientRequest, JsonRpcMessage,
    RequestId, RequestMetaObject, ServerResult,
};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::{Transport, async_rw::AsyncRwTransport};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

use crate::worker_client::ResponseDelivery;

// A blocked stdout cannot carry a correlated overload response. Closing the
// transport at this bound keeps rmcp's detached handler backlog finite.
const MAX_RESPONSE_GATED_CALLS: usize = 64;

/// The bound on `send` calls whose handlers have not finished.
const MAX_OUTSTANDING_SENDS: usize = 64;

#[derive(Clone, Default)]
pub(crate) struct ResponseDeliveries {
    state: Arc<Mutex<ResponseDeliveryState>>,
    gate_changed: Arc<Notify>,
    transport_closed: Arc<Notify>,
    /// Counts live admission nodes, including terminal nodes retained by a successor.
    admission_count: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(crate) struct ResponseDeliveryAdmission(Arc<ResponseDeliveryAdmissionState>);

#[derive(Clone)]
pub(crate) struct ResponseDeliveryCall(Arc<Mutex<ResponseDeliveryCallState>>);

pub(crate) struct ResponseDeliveryOperation {
    deliveries: ResponseDeliveries,
    request_id: RequestId,
    call: ResponseDeliveryCall,
    completed: bool,
}

pub(crate) enum ResponseDeliveryAdmissionError {
    Cancelled,
    Closed,
}

#[derive(Default)]
struct ResponseDeliveryState {
    // MCP requires request IDs to be unique throughout the session. Duplicate
    // IDs are invalid traffic, outside this delivery contract.
    // https://modelcontextprotocol.io/specification/2025-11-25/basic#requests
    active: HashMap<RequestId, ResponseDeliveryCall>,
    /// Every accepted reservation remains here until admission or cancellation.
    pending: HashMap<RequestId, u64>,
    next_admission_token: u64,
    /// Console response-write futures registered before they run on the service task.
    pending_writes: usize,
    /// All assembled protocol writes, including discovery, ping, and error responses.
    pending_protocol_writes: usize,
    /// Last waiting member of the wire-order chain, retained across cancellation gaps.
    admission_tail: Option<Arc<ResponseDeliveryAdmissionNode>>,
    /// Physical EOF is insufficient until the transport exhausts buffered requests.
    input_drained: bool,
    /// The latest message received a complete response. Used only before run()
    /// obtains an rmcp service, when discovery/ping can remain stateless.
    /// A legacy InitializeResult completes SDK startup immediately; the
    /// initialized notification does not gate it.
    bootstrap_response_delivered: bool,
    closed: bool,
}

struct ResponseDeliveryAdmissionState {
    deliveries: ResponseDeliveries,
    request_id: RequestId,
    token: u64,
    node: Mutex<Option<Arc<ResponseDeliveryAdmissionNode>>>,
}

struct ResponseDeliveryAdmissionNode {
    state: AtomicU8,
    /// Cleared only after the dependency settles so cancellation cannot break the chain.
    predecessor: Mutex<Option<Arc<ResponseDeliveryAdmissionNode>>>,
    changed: Notify,
    admission_count: Arc<AtomicUsize>,
}

struct ResponseDeliveryCallState {
    active: bool,
    operation_finished: bool,
    delivery: Option<ResponseDeliveryGroup>,
    admission: Option<Arc<ResponseDeliveryAdmissionNode>>,
}

/// One aggregate poll can claim output from several independent sessions.
/// They all share the MCP response's delivery outcome.
#[derive(Default)]
struct ResponseDeliveryGroup(Vec<ResponseDelivery>);

impl ResponseDeliveryGroup {
    fn delivered(self) {
        for delivery in self.0 {
            delivery.delivered();
        }
    }

    fn unclaimed(self) {
        for delivery in self.0 {
            delivery.unclaimed();
        }
    }
}

/// Keeps one console request registered until its response write finishes.
struct ResponseDeliveryWrite {
    deliveries: ResponseDeliveries,
    request_id: RequestId,
    call: ResponseDeliveryCall,
    finished: bool,
}

/// Retains delivery evidence even when the service drops an unpolled write future.
struct ProtocolWrite {
    deliveries: ResponseDeliveries,
    recording: Option<crate::transcript::Transcript>,
    request_id: Option<RequestId>,
    call_id: Option<u64>,
    delivered: bool,
}

impl Drop for ProtocolWrite {
    fn drop(&mut self) {
        if let (Some(recording), Some(id)) = (&self.recording, &self.request_id) {
            recording.delivery(&serde_json::json!(id), self.call_id, self.delivered);
        }
        self.deliveries.lock().pending_protocol_writes -= 1;
        self.deliveries.gate_changed.notify_waiters();
    }
}

impl ResponseDeliveries {
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub(crate) fn bootstrap_complete(&self) -> bool {
        let state = self.lock();
        state.input_drained && state.bootstrap_response_delivered
    }

    #[cfg(test)]
    fn start_response(&self, request_id: RequestId) -> ResponseDeliveryCall {
        self.admission_count.fetch_add(1, Ordering::AcqRel);
        let admission = Arc::new(ResponseDeliveryAdmissionNode::new(
            None,
            Arc::clone(&self.admission_count),
        ));
        admission.admit();
        let (call, replaced) = {
            let mut state = self.lock();
            Self::start_call(&mut state, request_id, Some(admission))
        };
        if let Some(replaced) = replaced {
            replaced.unclaimed();
        }
        {
            let mut state = call.lock();
            state.operation_finished = true;
            if !state.active {
                ResponseDeliveryCall::finish_admission(&mut state);
            }
        }
        call
    }

    /// Reserves transport order without retaining the request message.
    fn reserve(&self, request_id: RequestId) -> Option<ResponseDeliveryAdmission> {
        let (token, node) = {
            let mut state = self.lock();
            Self::prune_admission_tail(&mut state);
            if state.closed {
                return None;
            }
            let node = if !Self::reservation_gate_active(&state) {
                None
            } else {
                if self.admission_count.load(Ordering::Acquire) >= MAX_RESPONSE_GATED_CALLS {
                    return None;
                }
                self.admission_count.fetch_add(1, Ordering::AcqRel);
                let predecessor = state.admission_tail.clone();
                let node = Arc::new(ResponseDeliveryAdmissionNode::new(
                    predecessor,
                    Arc::clone(&self.admission_count),
                ));
                state.admission_tail = Some(Arc::clone(&node));
                Some(node)
            };
            let token = state.next_admission_token;
            state.next_admission_token = token
                .checked_add(1)
                .expect("response admission token space exhausted");
            state.pending.insert(request_id.clone(), token);
            (token, node)
        };
        let admission = ResponseDeliveryAdmission(Arc::new(ResponseDeliveryAdmissionState {
            deliveries: self.clone(),
            request_id,
            token,
            node: Mutex::new(node),
        }));
        self.gate_changed.notify_waiters();
        Some(admission)
    }

    fn start_call(
        state: &mut ResponseDeliveryState,
        request_id: RequestId,
        admission: Option<Arc<ResponseDeliveryAdmissionNode>>,
    ) -> (ResponseDeliveryCall, Option<ResponseDeliveryGroup>) {
        let call = ResponseDeliveryCall::new(!state.closed, admission);
        let replaced = if state.closed {
            None
        } else {
            state.active.insert(request_id, call.clone())
        };
        let delivery = replaced.and_then(|replaced| replaced.abandon());
        Self::prune_admission_tail(state);
        (call, delivery)
    }

    pub(crate) fn cancel(&self, request_id: &RequestId) {
        let delivery = {
            let mut state = self.lock();
            // A pending reuse of the ID is newer than an active call whose
            // already-visible response write has not settled yet.
            let pending = state.pending.remove(request_id).is_some();
            let delivery = (!pending)
                .then(|| state.active.get(request_id).cloned())
                .flatten()
                .and_then(|call| {
                    let (remove, delivery) = call.cancel();
                    if remove {
                        state.active.remove(request_id);
                    }
                    delivery
                });
            Self::prune_admission_tail(&mut state);
            delivery
        };
        if let Some(delivery) = delivery {
            delivery.unclaimed();
        }
        self.gate_changed.notify_waiters();
    }

    pub(crate) fn register(&self, call: &ResponseDeliveryCall, delivery: ResponseDelivery) {
        let rejected = {
            let state = self.lock();
            let current = state.active.values().find(|current| current.is(call));
            if current.is_some() {
                call.register(delivery).err()
            } else {
                Some(delivery)
            }
        };
        if let Some(delivery) = rejected {
            delivery.unclaimed();
        }
    }

    fn finish_operation(
        &self,
        request_id: &RequestId,
        expected: &ResponseDeliveryCall,
        completed: bool,
    ) {
        let delivery = {
            let mut state = self.lock();
            let Some(current) = state.active.get(request_id) else {
                return;
            };
            if !current.is(expected) {
                return;
            }
            let (remove, delivery) = if completed {
                expected.complete_operation()
            } else {
                (true, expected.abandon())
            };
            if remove {
                state.active.remove(request_id);
            }
            Self::prune_admission_tail(&mut state);
            delivery
        };
        if let Some(delivery) = delivery {
            delivery.unclaimed();
        }
        self.gate_changed.notify_waiters();
    }

    fn write(&self, request_id: &RequestId) -> Option<ResponseDeliveryWrite> {
        let call = {
            let mut state = self.lock();
            let call = state.active.get(request_id).cloned()?;
            call.begin_write();
            state.pending_writes += 1;
            call
        };
        Some(ResponseDeliveryWrite {
            deliveries: self.clone(),
            request_id: request_id.clone(),
            call,
            finished: false,
        })
    }

    fn close(&self) {
        let deliveries = {
            let mut state = self.lock();
            state.closed = true;
            let deliveries = state
                .active
                .drain()
                .filter_map(|(_, call)| call.abandon())
                .collect::<Vec<_>>();
            state.pending.clear();
            state.admission_tail = None;
            deliveries
        };
        self.transport_closed.notify_waiters();
        self.gate_changed.notify_waiters();
        for delivery in deliveries {
            delivery.unclaimed();
        }
    }

    /// Drain queued input and its responses after physical EOF, with a bound
    /// for clients that stop reading their output pipe.
    pub(crate) async fn settle_before_close(&self, deadline: Instant) {
        let settled = async {
            loop {
                let changed = self.gate_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.lock();
                    // A startup EOF observer can run before the transport has
                    // framed queued requests and reserved their responses.
                    if state.closed
                        || (state.input_drained
                            && state.active.is_empty()
                            && state.pending.is_empty()
                            && state.pending_writes == 0
                            && state.pending_protocol_writes == 0)
                    {
                        return;
                    }
                }
                changed.await;
            }
        };
        let _ = tokio::time::timeout_at(deadline.into(), settled).await;
        self.close();
        // Closing wakes blocked writes. Let their delivery observations settle
        // before the caller finalizes recording, including on a current-thread runtime.
        loop {
            let changed = self.gate_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let settled = {
                let state = self.lock();
                state.pending_writes == 0 && state.pending_protocol_writes == 0
            };
            if settled {
                return;
            }
            changed.await;
        }
    }

    fn take_write_delivery(
        &self,
        request_id: &RequestId,
        expected: &ResponseDeliveryCall,
    ) -> Option<ResponseDeliveryGroup> {
        let mut state = self.lock();
        let current = state.active.get(request_id)?;
        if !current.is(expected) {
            return None;
        }
        let delivery = state
            .active
            .remove(request_id)
            .and_then(|call| call.finish_write());
        Self::prune_admission_tail(&mut state);
        delivery
    }

    fn settle_write(&self) {
        {
            let mut state = self.lock();
            assert!(
                state.pending_writes > 0,
                "a response write must be pending before it settles"
            );
            state.pending_writes -= 1;
        }
        self.gate_changed.notify_waiters();
    }

    fn response_gate_active(state: &ResponseDeliveryState) -> bool {
        state.pending_writes > 0
            || state
                .active
                .values()
                .any(ResponseDeliveryCall::blocks_new_admissions)
    }

    fn reservation_gate_active(state: &ResponseDeliveryState) -> bool {
        Self::response_gate_active(state) || state.admission_tail.is_some()
    }

    async fn wait_for_close(&self) {
        loop {
            let transport_closed = self.transport_closed.notified();
            if self.lock().closed {
                return;
            }
            transport_closed.await;
        }
    }

    fn prune_admission_tail(state: &mut ResponseDeliveryState) {
        state.admission_tail = state.admission_tail.take().and_then(|tail| tail.waiting());
    }

    fn owns_pending(
        state: &ResponseDeliveryState,
        admission: &ResponseDeliveryAdmissionState,
    ) -> bool {
        state
            .pending
            .get(&admission.request_id)
            .is_some_and(|token| *token == admission.token)
    }

    fn unregister_pending(&self, request_id: &RequestId, token: u64) {
        {
            let mut state = self.lock();
            if state
                .pending
                .get(request_id)
                .is_some_and(|current| *current == token)
            {
                state.pending.remove(request_id);
            }
            Self::prune_admission_tail(&mut state);
        }
        self.gate_changed.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ResponseDeliveryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ResponseDeliveryAdmission {
    /// Registers the call when every earlier gated call and response write has settled.
    pub(crate) async fn admit(
        self,
    ) -> Result<(ResponseDeliveryCall, ResponseDeliveryOperation), ResponseDeliveryAdmissionError>
    {
        let node = self.0.node();
        let queued = node.is_some();
        if let Some(node) = &node {
            loop {
                let gate_changed = self.0.deliveries.gate_changed.notified();
                {
                    let state = self.0.deliveries.lock();
                    if state.closed {
                        return Err(ResponseDeliveryAdmissionError::Closed);
                    }
                    if !ResponseDeliveries::owns_pending(&state, &self.0) {
                        return Err(ResponseDeliveryAdmissionError::Cancelled);
                    }
                }
                tokio::select! {
                    _ = node.wait_for_predecessor() => break,
                    _ = gate_changed => {}
                }
            }
        }
        loop {
            let gate_changed = self.0.deliveries.gate_changed.notified();
            let admission = {
                let mut state = self.0.deliveries.lock();
                if state.closed {
                    return Err(ResponseDeliveryAdmissionError::Closed);
                }
                if !ResponseDeliveries::owns_pending(&state, &self.0) {
                    return Err(ResponseDeliveryAdmissionError::Cancelled);
                }
                if !queued || !ResponseDeliveries::response_gate_active(&state) {
                    state.pending.remove(&self.0.request_id);
                    let node = node.as_ref().map(|_| {
                        self.0
                            .take_node()
                            .expect("queued admission must retain its node")
                    });
                    if let Some(node) = &node {
                        node.admit();
                    }
                    Some(ResponseDeliveries::start_call(
                        &mut state,
                        self.0.request_id.clone(),
                        node,
                    ))
                } else {
                    None
                }
            };
            if let Some((call, replaced)) = admission {
                if let Some(replaced) = replaced {
                    replaced.unclaimed();
                }
                self.0.deliveries.gate_changed.notify_waiters();
                let operation = ResponseDeliveryOperation {
                    deliveries: self.0.deliveries.clone(),
                    request_id: self.0.request_id.clone(),
                    call: call.clone(),
                    completed: false,
                };
                return Ok((call, operation));
            }
            gate_changed.await;
        }
    }
}

impl ResponseDeliveryAdmissionState {
    fn node(&self) -> Option<Arc<ResponseDeliveryAdmissionNode>> {
        self.lock_node().clone()
    }

    fn take_node(&self) -> Option<Arc<ResponseDeliveryAdmissionNode>> {
        self.lock_node().take()
    }

    fn lock_node(&self) -> std::sync::MutexGuard<'_, Option<Arc<ResponseDeliveryAdmissionNode>>> {
        self.node
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for ResponseDeliveryAdmissionState {
    fn drop(&mut self) {
        if let Some(node) = self
            .node
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            node.skip();
        }
        self.deliveries
            .unregister_pending(&self.request_id, self.token);
    }
}

impl ResponseDeliveryAdmissionNode {
    const WAITING: u8 = 0;
    const ADMITTED: u8 = 1;
    const FINISHED: u8 = 2;
    const SKIPPED: u8 = 3;

    fn new(predecessor: Option<Arc<Self>>, admission_count: Arc<AtomicUsize>) -> Self {
        Self {
            state: AtomicU8::new(Self::WAITING),
            predecessor: Mutex::new(predecessor),
            changed: Notify::new(),
            admission_count,
        }
    }

    fn admit(&self) {
        self.transition(Self::WAITING, Self::ADMITTED);
    }

    fn finish(&self) {
        self.transition(Self::ADMITTED, Self::FINISHED);
    }

    fn skip(&self) {
        self.transition(Self::WAITING, Self::SKIPPED);
    }

    fn transition(&self, expected: u8, next: u8) {
        if self
            .state
            .compare_exchange(expected, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.changed.notify_waiters();
        }
    }

    /// Finds the last waiting node without restoring an admitted predecessor as the tail.
    fn waiting(self: &Arc<Self>) -> Option<Arc<Self>> {
        let mut current = Arc::clone(self);
        loop {
            match current.state.load(Ordering::Acquire) {
                Self::WAITING => return Some(current),
                Self::ADMITTED | Self::FINISHED => return None,
                Self::SKIPPED => {
                    current = current.predecessor()?;
                }
                _ => unreachable!("unknown response admission state"),
            }
        }
    }

    async fn wait_for_predecessor(&self) {
        if let Some(predecessor) = self.predecessor() {
            predecessor.wait().await;
            let mut current = self.lock_predecessor();
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &predecessor))
            {
                *current = None;
            }
        }
    }

    async fn wait(self: &Arc<Self>) {
        let mut current = Arc::clone(self);
        loop {
            let changed = current.changed.notified();
            match current.state.load(Ordering::Acquire) {
                Self::FINISHED => return,
                Self::SKIPPED => {
                    let predecessor = current.predecessor();
                    drop(changed);
                    let Some(predecessor) = predecessor else {
                        return;
                    };
                    current = predecessor;
                }
                Self::WAITING | Self::ADMITTED => changed.await,
                _ => unreachable!("unknown response admission state"),
            }
        }
    }

    fn predecessor(&self) -> Option<Arc<Self>> {
        self.lock_predecessor().clone()
    }

    fn lock_predecessor(&self) -> std::sync::MutexGuard<'_, Option<Arc<Self>>> {
        self.predecessor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for ResponseDeliveryAdmissionNode {
    fn drop(&mut self) {
        let previous = self.admission_count.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "a reserved admission must occupy one slot");
    }
}

impl ResponseDeliveryCall {
    fn new(active: bool, admission: Option<Arc<ResponseDeliveryAdmissionNode>>) -> Self {
        Self(Arc::new(Mutex::new(ResponseDeliveryCallState {
            active,
            operation_finished: false,
            delivery: None,
            admission,
        })))
    }

    fn register(&self, delivery: ResponseDelivery) -> Result<(), ResponseDelivery> {
        let mut state = self.lock();
        if !state.active {
            return Err(delivery);
        }
        assert!(
            !state.operation_finished,
            "response delivery must register before operation completion"
        );
        state
            .delivery
            .get_or_insert_with(Default::default)
            .0
            .push(delivery);
        Ok(())
    }

    fn cancel(&self) -> (bool, Option<ResponseDeliveryGroup>) {
        let mut state = self.lock();
        state.active = false;
        let delivery = state.delivery.take();
        if state.operation_finished {
            Self::finish_admission(&mut state);
            (true, delivery)
        } else {
            (false, delivery)
        }
    }

    fn complete_operation(&self) -> (bool, Option<ResponseDeliveryGroup>) {
        let mut state = self.lock();
        assert!(
            !state.operation_finished,
            "a response operation can complete only once"
        );
        state.operation_finished = true;
        if !state.active {
            Self::finish_admission(&mut state);
            return (true, state.delivery.take());
        }
        (false, None)
    }

    fn abandon(&self) -> Option<ResponseDeliveryGroup> {
        let mut state = self.lock();
        state.active = false;
        state.operation_finished = true;
        Self::finish_admission(&mut state);
        state.delivery.take()
    }

    fn finish_write(&self) -> Option<ResponseDeliveryGroup> {
        let mut state = self.lock();
        state.active = false;
        Self::finish_admission(&mut state);
        state.delivery.take()
    }

    fn finish_admission(state: &mut ResponseDeliveryCallState) {
        if let Some(admission) = state.admission.take() {
            admission.finish();
        }
    }

    fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn blocks_new_admissions(&self) -> bool {
        let state = self.lock();
        state.operation_finished
            || state.delivery.is_some()
            || (state.admission.is_some() && !state.active)
    }

    fn begin_write(&self) {
        let state = self.lock();
        assert!(
            state.operation_finished,
            "response writing must follow operation completion"
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ResponseDeliveryCallState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ResponseDeliveryOperation {
    pub(crate) fn complete(mut self) {
        self.completed = true;
        self.deliveries
            .finish_operation(&self.request_id, &self.call, true);
    }
}

impl Drop for ResponseDeliveryOperation {
    fn drop(&mut self) {
        if !self.completed {
            self.deliveries
                .finish_operation(&self.request_id, &self.call, false);
        }
    }
}

impl ResponseDeliveryWrite {
    fn delivered(mut self) {
        self.finish(ResponseDeliveryGroup::delivered);
    }

    fn unclaimed(mut self) {
        self.finish(ResponseDeliveryGroup::unclaimed);
    }

    fn finish(&mut self, settle_delivery: impl FnOnce(ResponseDeliveryGroup)) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(delivery) = self
            .deliveries
            .take_write_delivery(&self.request_id, &self.call)
        {
            settle_delivery(delivery);
        }
        self.deliveries.settle_write();
    }
}

impl Drop for ResponseDeliveryWrite {
    fn drop(&mut self) {
        self.finish(ResponseDeliveryGroup::unclaimed);
    }
}

type SendObserver = dyn Fn(&mut CallToolRequest) + Send;

/// One call queued for capture at MCP receipt, carried to its handler.
#[derive(Clone)]
pub(crate) struct RecordingCall {
    id: u64,
    call: crate::transcript::Call,
    // Closing the last sender releases capture ownership, independently of
    // cancelled delivery admission.
    _completion: Arc<tokio::sync::oneshot::Sender<()>>,
}

impl RecordingCall {
    // rmcp responses have no extensions. Carry instance identity through its
    // handler channel, then remove this envelope before capture or wire encoding.
    // Cancellation can remove delivery ownership before a response is captured.
    const RESPONSE_ID: &'static str = "mcp-console/internal/recording-call";

    pub(crate) fn call(&self) -> crate::transcript::Call {
        self.call.clone()
    }

    pub(crate) fn response(
        self,
        mut response: Result<CallToolResponse, rmcp::ErrorData>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        match &mut response {
            Ok(CallToolResponse::Complete(result)) => {
                let identity = serde_json::json!({"call_id": self.id, "meta": result.meta.take()});
                let mut meta = rmcp::model::MetaObject::new();
                meta.insert(Self::RESPONSE_ID.into(), identity);
                result.meta = Some(meta);
            }
            Err(error) => {
                let mut identity = serde_json::json!({Self::RESPONSE_ID: self.id});
                if let Some(data) = error.data.take() {
                    identity["data"] = data;
                }
                error.data = Some(identity);
            }
            Ok(_) => unreachable!("send returns complete results"),
        }
        response
    }

    fn take_response_id(message: &mut TxJsonRpcMessage<RoleServer>) -> Option<u64> {
        let identity = match message {
            JsonRpcMessage::Response(response) => {
                let ServerResult::CallToolResult(result) = &mut response.result else {
                    return None;
                };
                let mut identity = result.meta.as_mut()?.remove(Self::RESPONSE_ID)?;
                result.meta = serde_json::from_value(identity["meta"].take())
                    .expect("original response metadata");
                identity["call_id"].take()
            }
            JsonRpcMessage::Error(error) => {
                let identity = error.error.data.as_mut()?;
                let id = identity.get(Self::RESPONSE_ID)?.clone();
                error.error.data = identity
                    .as_object_mut()
                    .expect("recorded error")
                    .remove("data");
                id
            }
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => return None,
        };
        Some(identity.as_u64().expect("recorded response call ID"))
    }
}

/// Handlers that can still queue a result survive delivery cancellation.
#[derive(Clone, Default)]
pub(crate) struct RecordingQueue(Arc<Mutex<RecordingQueueState>>);

#[derive(Default)]
struct RecordingQueueState {
    closed: bool,
    pending: Vec<tokio::sync::oneshot::Receiver<()>>,
}

impl RecordingQueue {
    pub(crate) async fn close_and_settle(&self) {
        let pending = {
            let mut queue = self.0.lock().expect("recording queue lock");
            queue.closed = true;
            std::mem::take(&mut queue.pending)
        };
        for completion in pending {
            // Sender closure means all handler owners have retired; it does
            // not fabricate a successful tool result for an abandoned call.
            let _ = completion.await;
        }
    }
}

pub(crate) struct ServerTransport<R: AsyncRead, W: AsyncWrite> {
    inner: AsyncRwTransport<RoleServer, R, W>,
    deliveries: ResponseDeliveries,
    send_observer: Option<Box<SendObserver>>,
    recording: Option<crate::transcript::Transcript>,
    recording_queue: RecordingQueue,
}

impl<R, W> ServerTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    pub(crate) fn new(read: R, write: W, deliveries: ResponseDeliveries) -> Self {
        Self {
            // The SDK's default 16 MiB line limit drops a longer request
            // without a response, leaving its caller waiting. The peer is the
            // launching client, so accept its requests at any length.
            inner: AsyncRwTransport::new_server(read, write).with_max_line_length(usize::MAX),
            deliveries,
            send_observer: None,
            recording: None,
            recording_queue: RecordingQueue::default(),
        }
    }

    pub(crate) fn with_recording(mut self, recording: crate::transcript::Transcript) -> Self {
        self.recording = Some(recording);
        self
    }

    pub(crate) fn recording_queue(&self) -> RecordingQueue {
        self.recording_queue.clone()
    }

    /// Observe accepted sends in wire order, before asynchronous handler dispatch.
    pub(crate) fn with_send_observer(
        mut self,
        observer: impl Fn(&mut CallToolRequest) + Send + 'static,
    ) -> Self {
        self.send_observer = Some(Box::new(observer));
        self
    }
}

impl<R, W> Transport<RoleServer> for ServerTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        mut item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let call_id = RecordingCall::take_response_id(&mut item);
        let response = matches!(
            &item,
            JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_)
        );
        let delivery = match &item {
            JsonRpcMessage::Response(response) => self.deliveries.write(&response.id),
            JsonRpcMessage::Error(error) => error
                .id
                .as_ref()
                .and_then(|request_id| self.deliveries.write(request_id)),
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        // rmcp's EOF drain can forward a handler result after cancellation.
        // Every console tool result must still own its response reservation.
        let cancelled_tool_result = delivery.is_none()
            && matches!(&item, JsonRpcMessage::Response(response)
                if matches!(response.result, ServerResult::CallToolResult(_)));
        let recording = self.recording.clone();
        let request_id = match &item {
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        if let Some(recording) = &recording {
            match serde_json::to_value(&item) {
                Ok(mut message) => {
                    if let Some(id) = call_id {
                        for field in ["result", "error"] {
                            if let Some(payload) = message.get_mut(field) {
                                *payload = serde_json::json!({"tool_result_call_id": id});
                            }
                        }
                    }
                    recording.protocol("assembled", message);
                }
                Err(error) => recording.disable(format!("protocol capture failed: {error}")),
            }
        }
        self.deliveries.lock().pending_protocol_writes += 1;
        let mut write = ProtocolWrite {
            deliveries: self.deliveries.clone(),
            recording,
            request_id,
            call_id,
            delivered: false,
        };
        let send = (!cancelled_tool_result).then(|| self.inner.send(item));
        let deliveries = self.deliveries.clone();
        async move {
            let Some(send) = send else {
                drop(write);
                return Ok(());
            };
            // Preserve a ready final response after input EOF, but abandon a
            // blocked stdout write so transport shutdown remains bounded.
            let result = tokio::select! {
                biased;
                result = send => result,
                _ = deliveries.wait_for_close() => {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                },
            };
            write.delivered = result.is_ok();
            drop(write);
            if response && result.is_ok() {
                deliveries.lock().bootstrap_response_delivered = true;
            }
            if let Some(delivery) = delivery {
                if result.is_ok() {
                    delivery.delivered();
                } else {
                    delivery.unclaimed();
                }
            }
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        if let Some(recording) = &self.recording {
            // Handlers queue their journal operations without waiting, so
            // intake is where a burst is held to the recording writer's pace.
            tokio::select! {
                biased;
                _ = self.deliveries.wait_for_close() => return None,
                () = recording.paced() => {}
            }
        }
        let mut message = tokio::select! {
            biased;
            _ = self.deliveries.wait_for_close() => return None,
            message = self.inner.receive() => message,
        };
        // Serialize receipt/capture enrollment with shutdown's final cut.
        // This lock protects only queueing, never recorder locks or journal I/O.
        let mut queue = self.recording_queue.0.lock().expect("recording queue lock");
        if queue.closed {
            return None;
        }
        let mut receipt = None;
        if let (Some(recording), Some(JsonRpcMessage::Request(request))) =
            (&self.recording, &message)
            && let ClientRequest::CallToolRequest(call) = &request.request
            && call.params.name.as_ref() == "send"
        {
            // A send can stay outstanding without a deadline, and its handler
            // retains its request. Closing the transport at this bound keeps
            // those handlers finite whatever the recording writer's pace.
            queue.pending.retain_mut(|completion| {
                matches!(
                    completion.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                )
            });
            if queue.pending.len() >= MAX_OUTSTANDING_SENDS {
                // The refused call gets no handler. Keep its received frame.
                match serde_json::to_value(&message) {
                    Ok(message) => recording.protocol("received", message),
                    Err(error) => recording.disable(format!("protocol capture failed: {error}")),
                }
                eprintln!(
                    "mcp-console: {MAX_OUTSTANDING_SENDS} send calls are outstanding; closing MCP input"
                );
                self.deliveries.close();
                return None;
            }
            let session = call
                .params
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("session"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("default");
            let recorder = recording.for_session(session);
            receipt = Some((recorder.receive_call(&call.params), recorder));
        }
        if let (Some(recording), Some(message)) = (&self.recording, &message) {
            let mut captured = serde_json::to_value(message);
            if let Some((receipt, _)) = &receipt
                && let Ok(message) = &mut captured
            {
                message["params"] = serde_json::json!({"tool_call_id": receipt.id});
            }
            match captured {
                Ok(message) => recording.protocol("received", message),
                Err(error) => recording.disable(format!("protocol capture failed: {error}")),
            }
        }
        // Queue the call after its receipt and in receive order. Neither waits
        // for the recording writer.
        if let Some((receipt, recorder)) = receipt
            && let Some(JsonRpcMessage::Request(request)) = &mut message
            && let ClientRequest::CallToolRequest(call) = &mut request.request
        {
            let id = receipt.id;
            let meta = call
                .extensions
                .get::<RequestMetaObject>()
                .cloned()
                .unwrap_or_default();
            let recorded = recorder.begin(receipt, &request.id, &meta, &call.params);
            let (completion, finished) = tokio::sync::oneshot::channel();
            queue.pending.push(finished);
            call.extensions.insert(RecordingCall {
                id,
                call: recorded,
                _completion: Arc::new(completion),
            });
        }
        drop(queue);
        if message.is_some() {
            self.deliveries.lock().bootstrap_response_delivered = false;
        }
        match &mut message {
            Some(JsonRpcMessage::Request(request)) => {
                if let ClientRequest::CallToolRequest(call) = &mut request.request
                    && call.params.name.as_ref() == "send"
                {
                    let Some(admission) = self.deliveries.reserve(request.id.clone()) else {
                        // Under stdout backpressure another response is not reliable,
                        // so overload retires the connection and its worker session.
                        self.deliveries.close();
                        return None;
                    };
                    call.extensions.insert(admission);
                    if let Some(observer) = &self.send_observer {
                        observer(call);
                    }
                }
            }
            Some(JsonRpcMessage::Notification(notification)) => {
                if let ClientNotification::CancelledNotification(cancelled) =
                    &notification.notification
                    && let Some(request_id) = &cancelled.params.request_id
                {
                    self.deliveries.cancel(request_id);
                }
            }
            Some(JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_)) => {}
            None => {
                self.deliveries.lock().input_drained = true;
                self.deliveries.gate_changed.notify_waiters();
            }
        }
        message
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.deliveries.close();
        self.inner.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ServerResult;
    use tokio::io::AsyncWriteExt;

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    const PING_REQUEST: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}\n";
    const SEND_REQUEST: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"send\",\"arguments\":{}},\"id\":1}\n";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    const CANCEL_REQUEST_9: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":9}}\n";

    fn request_id(value: i64) -> RequestId {
        RequestId::Number(value)
    }

    fn current_call(
        deliveries: &ResponseDeliveries,
        request_id: &RequestId,
    ) -> Option<ResponseDeliveryCall> {
        deliveries.lock().active.get(request_id).cloned()
    }

    fn is_active(call: &ResponseDeliveryCall) -> bool {
        call.lock().active
    }

    struct PendingWrite {
        started: Arc<Notify>,
    }

    impl AsyncWrite for PendingWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            _buffer: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.started.notify_one();
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn keeps_call_registered_until_response_write_finishes() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let call = deliveries.start_response(request_id.clone());
        let write = deliveries.write(&request_id).unwrap();

        assert!(current_call(&deliveries, &request_id).is_some_and(|current| current.is(&call)));
        assert!(is_active(&call));

        write.delivered();

        assert!(current_call(&deliveries, &request_id).is_none());
        assert!(!is_active(&call));
    }

    #[test]
    fn old_write_cannot_remove_reused_request_id() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let old_call = deliveries.start_response(request_id.clone());
        let old_write = deliveries.write(&request_id).unwrap();
        let new_call = deliveries.start_response(request_id.clone());

        assert!(!is_active(&old_call));
        assert!(
            current_call(&deliveries, &request_id).is_some_and(|current| current.is(&new_call))
        );

        old_write.delivered();

        assert!(
            current_call(&deliveries, &request_id).is_some_and(|current| current.is(&new_call))
        );
        assert!(is_active(&new_call));
    }

    #[test]
    fn cancellation_wins_over_in_flight_write() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let call = deliveries.start_response(request_id.clone());
        let write = deliveries.write(&request_id).unwrap();

        deliveries.cancel(&request_id);

        assert!(current_call(&deliveries, &request_id).is_none());
        assert!(!is_active(&call));

        write.delivered();
        assert!(current_call(&deliveries, &request_id).is_none());
    }

    // macOS/Linux use the public response_admission transcript (and the existing
    // output cancellation case). Keep this proof where native write gates are
    // unavailable; its body and transport configuration remain unchanged.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[tokio::test]
    async fn response_gate_does_not_delay_cancellation_notifications() {
        let deliveries = ResponseDeliveries::default();
        deliveries.lock().pending_writes = 1;
        let cancelled_id = request_id(9);
        let cancelled = deliveries.reserve(cancelled_id.clone()).unwrap();
        let (mut input, read) = tokio::io::duplex(1024);
        input.write_all(PING_REQUEST).await.unwrap();
        input.write_all(CANCEL_REQUEST_9).await.unwrap();
        let mut transport = ServerTransport::new(read, tokio::io::sink(), deliveries);

        let first = {
            let mut receive = Box::pin(transport.receive());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            match std::future::Future::poll(receive.as_mut(), &mut context) {
                std::task::Poll::Ready(Some(message)) => message,
                _ => panic!("the gated request should reach rmcp immediately"),
            }
        };
        assert!(matches!(first, JsonRpcMessage::Request(_)));

        let second = {
            let mut receive = Box::pin(transport.receive());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            match std::future::Future::poll(receive.as_mut(), &mut context) {
                std::task::Poll::Ready(Some(message)) => message,
                _ => panic!("cancellation should not wait for the response write"),
            }
        };
        assert!(matches!(second, JsonRpcMessage::Notification(_)));
        assert!(matches!(
            cancelled.admit().await,
            Err(ResponseDeliveryAdmissionError::Cancelled)
        ));
        assert!(current_call(&transport.deliveries, &cancelled_id).is_none());
    }

    // Public owner: client_server/server/test_response_admission on macOS/Linux.
    // Other targets retain this adversarial successor-first polling proof.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[tokio::test]
    async fn response_gate_skips_cancelled_calls_without_reordering() {
        let deliveries = ResponseDeliveries::default();
        deliveries.lock().pending_writes = 1;
        let first = deliveries.reserve(request_id(1)).unwrap();
        let cancelled_id = request_id(9);
        let cancelled = deliveries.reserve(cancelled_id.clone()).unwrap();
        let second = deliveries.reserve(request_id(2)).unwrap();
        let mut first = Box::pin(first.admit());
        let mut cancelled = Box::pin(cancelled.admit());
        let mut second = Box::pin(second.admit());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        assert!(std::future::Future::poll(second.as_mut(), &mut context).is_pending());
        assert!(std::future::Future::poll(cancelled.as_mut(), &mut context).is_pending());
        assert!(std::future::Future::poll(first.as_mut(), &mut context).is_pending());
        deliveries.cancel(&cancelled_id);
        assert!(matches!(
            std::future::Future::poll(cancelled.as_mut(), &mut context),
            std::task::Poll::Ready(Err(ResponseDeliveryAdmissionError::Cancelled))
        ));
        drop(cancelled);
        deliveries.settle_write();
        assert!(std::future::Future::poll(second.as_mut(), &mut context).is_pending());
        let (first_call, first_operation) =
            match std::future::Future::poll(first.as_mut(), &mut context) {
                std::task::Poll::Ready(Ok(call)) => call,
                _ => panic!("the first reserved call should be admitted"),
            };

        first_operation.complete();
        let first_write = deliveries.write(&request_id(1)).unwrap();
        assert!(std::future::Future::poll(second.as_mut(), &mut context).is_pending());
        first_write.delivered();
        assert!(matches!(
            std::future::Future::poll(second.as_mut(), &mut context),
            std::task::Poll::Ready(Ok((_call, _operation)))
        ));
        assert!(!is_active(&first_call));
    }

    #[tokio::test]
    async fn cancelled_admitted_call_holds_successor_until_its_operation_stops() {
        let deliveries = ResponseDeliveries::default();
        deliveries.lock().pending_writes = 1;
        let cancelled_id = request_id(1);
        let cancelled = deliveries.reserve(cancelled_id.clone()).unwrap();
        let successor = deliveries.reserve(request_id(2)).unwrap();
        let mut cancelled = Box::pin(cancelled.admit());
        let mut successor = Box::pin(successor.admit());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        deliveries.settle_write();
        let (call, operation) = match std::future::Future::poll(cancelled.as_mut(), &mut context) {
            std::task::Poll::Ready(Ok(admitted)) => admitted,
            _ => panic!("the first reserved call should be admitted"),
        };
        assert!(std::future::Future::poll(successor.as_mut(), &mut context).is_pending());

        deliveries.cancel(&cancelled_id);
        assert!(!is_active(&call));
        assert!(current_call(&deliveries, &cancelled_id).is_some());
        assert!(std::future::Future::poll(successor.as_mut(), &mut context).is_pending());

        drop(operation);
        assert!(current_call(&deliveries, &cancelled_id).is_none());
        assert!(matches!(
            std::future::Future::poll(successor.as_mut(), &mut context),
            std::task::Poll::Ready(Ok((_call, _operation)))
        ));
    }

    #[tokio::test]
    async fn completed_initial_operation_holds_successor_until_its_response_write() {
        let deliveries = ResponseDeliveries::default();
        let first_id = request_id(1);
        let admission = deliveries.reserve(first_id.clone()).unwrap();
        let (call, operation) = match admission.admit().await {
            Ok(admitted) => admitted,
            Err(_) => panic!("the initial call should be admitted"),
        };

        operation.complete();
        assert!(current_call(&deliveries, &first_id).is_some_and(|current| current.is(&call)));
        assert!(call.blocks_new_admissions());
        let successor = deliveries.reserve(request_id(2)).unwrap();
        let mut successor = Box::pin(successor.admit());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(successor.as_mut(), &mut context).is_pending());

        let write = deliveries.write(&first_id).unwrap();
        assert_eq!(deliveries.lock().pending_writes, 1);
        assert!(std::future::Future::poll(successor.as_mut(), &mut context).is_pending());
        write.delivered();

        assert_eq!(deliveries.lock().pending_writes, 0);
        assert!(current_call(&deliveries, &first_id).is_none());
        assert!(matches!(
            std::future::Future::poll(successor.as_mut(), &mut context),
            std::task::Poll::Ready(Ok((_call, _operation)))
        ));
    }

    #[test]
    fn bounds_response_gated_reservations() {
        let deliveries = ResponseDeliveries::default();
        let blocker_id = request_id(0);
        deliveries.start_response(blocker_id.clone());
        let blocker_write = deliveries.write(&blocker_id).unwrap();
        let reservations = (1..MAX_RESPONSE_GATED_CALLS)
            .map(|id| {
                deliveries
                    .reserve(request_id(id as i64))
                    .expect("reservation within the response-gated bound")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            deliveries.admission_count.load(Ordering::Acquire),
            MAX_RESPONSE_GATED_CALLS
        );
        assert!(
            deliveries
                .reserve(request_id(MAX_RESPONSE_GATED_CALLS as i64))
                .is_none()
        );

        drop(reservations);
        blocker_write.delivered();
        assert_eq!(deliveries.admission_count.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn eof_preserves_pending_response_write_until_transport_closes() {
        let deliveries = ResponseDeliveries::default();
        let response_id = request_id(1);
        let admission = deliveries.reserve(response_id.clone()).unwrap();
        let (call, operation) = match admission.admit().await {
            Ok(admitted) => admitted,
            Err(_) => panic!("the initial call should be admitted"),
        };
        operation.complete();
        let successor = deliveries.reserve(request_id(2)).unwrap();

        let (input, read) = tokio::io::duplex(1);
        let started = Arc::new(Notify::new());
        let writer = PendingWrite {
            started: Arc::clone(&started),
        };
        let mut transport = ServerTransport::new(read, writer, deliveries.clone());
        let response = JsonRpcMessage::response(ServerResult::empty(()), response_id.clone());
        let send = tokio::spawn(transport.send(response));

        tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
            .await
            .expect("response writer should be reached");
        assert_eq!(deliveries.lock().pending_writes, 1);
        assert!(current_call(&deliveries, &response_id).is_some_and(|current| current.is(&call)));

        drop(input);
        assert!(transport.receive().await.is_none());
        assert_eq!(deliveries.lock().pending_writes, 1);
        assert!(current_call(&deliveries, &response_id).is_some());
        deliveries.close();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), send)
            .await
            .expect("response write should be cancelled")
            .expect("response write task should finish")
            .expect_err("closing the transport should cancel the pending response write");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(deliveries.lock().pending_writes, 0);
        assert!(current_call(&deliveries, &response_id).is_none());
        assert!(!is_active(&call));
        assert!(matches!(
            successor.admit().await,
            Err(ResponseDeliveryAdmissionError::Closed)
        ));
    }

    #[tokio::test]
    async fn eof_waits_for_an_accepted_unadmitted_send() {
        let deliveries = ResponseDeliveries::default();
        let (mut input, read) = tokio::io::duplex(1024);
        input.write_all(SEND_REQUEST).await.unwrap();
        drop(input);
        let mut transport = ServerTransport::new(read, tokio::io::sink(), deliveries.clone());
        let Some(JsonRpcMessage::Request(mut request)) = transport.receive().await else {
            panic!("the send request should be accepted before EOF");
        };
        let ClientRequest::CallToolRequest(ref mut call) = request.request else {
            panic!("the request should call send");
        };
        let admission = call
            .extensions
            .remove::<ResponseDeliveryAdmission>()
            .expect("send must carry a delivery reservation");
        assert!(transport.receive().await.is_none());

        let mut settle = Box::pin(
            deliveries.settle_before_close(Instant::now() + std::time::Duration::from_secs(30)),
        );
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(settle.as_mut(), &mut context).is_pending());

        let (_call, operation) = match admission.admit().await {
            Ok(admitted) => admitted,
            Err(_) => panic!("accepted call must be admitted"),
        };
        operation.complete();
        let response = JsonRpcMessage::response(ServerResult::empty(()), request.id);
        transport
            .send(response)
            .await
            .expect("response should reach stdout");
        settle.await;
        assert!(deliveries.lock().closed);
    }

    #[test]
    fn failed_write_releases_current_call() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let call = deliveries.start_response(request_id.clone());
        let write = deliveries.write(&request_id).unwrap();

        write.unclaimed();

        assert!(current_call(&deliveries, &request_id).is_none());
        assert!(!is_active(&call));
    }

    #[test]
    fn dropped_write_releases_current_call() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let call = deliveries.start_response(request_id.clone());
        let write = deliveries.write(&request_id).unwrap();

        drop(write);

        assert!(current_call(&deliveries, &request_id).is_none());
        assert!(!is_active(&call));
    }

    #[test]
    fn dropped_write_releases_only_its_own_call() {
        let deliveries = ResponseDeliveries::default();
        let request_id = request_id(1);
        let old_call = deliveries.start_response(request_id.clone());
        let old_write = deliveries.write(&request_id).unwrap();
        let new_call = deliveries.start_response(request_id.clone());

        drop(old_write);

        assert!(!is_active(&old_call));
        assert!(
            current_call(&deliveries, &request_id).is_some_and(|current| current.is(&new_call))
        );
    }

    #[test]
    fn close_releases_all_active_calls() {
        let deliveries = ResponseDeliveries::default();
        let first = deliveries.start_response(request_id(1));
        let second = deliveries.start_response(request_id(2));

        deliveries.close();

        assert!(deliveries.lock().active.is_empty());
        assert!(!is_active(&first));
        assert!(!is_active(&second));

        let after_close = deliveries.start_response(request_id(3));
        assert!(!is_active(&after_close));
        assert!(deliveries.lock().active.is_empty());
    }
}
