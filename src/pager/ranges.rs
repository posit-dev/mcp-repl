#[derive(Debug, Default, Clone)]
pub(super) struct RangeSet {
    ranges: Vec<(u64, u64)>,
}

impl RangeSet {
    pub(super) fn range_containing(&self, offset: u64) -> Option<(u64, u64)> {
        let idx = self.ranges.partition_point(|(start, _)| *start <= offset);
        if idx == 0 {
            return None;
        }
        let (start, end) = self.ranges[idx.saturating_sub(1)];
        (offset < end).then_some((start, end))
    }

    pub(super) fn covers(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        self.range_containing(start)
            .is_some_and(|(_, range_end)| range_end >= end)
    }

    pub(super) fn next_range_start_after(&self, offset: u64) -> Option<u64> {
        let idx = self.ranges.partition_point(|(start, _)| *start < offset);
        self.ranges.get(idx).map(|(start, _)| *start)
    }

    pub(super) fn insert(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }
        let mut idx = 0usize;
        while idx < self.ranges.len() {
            let (range_start, range_end) = self.ranges[idx];
            if end < range_start {
                break;
            }
            if start > range_end {
                idx += 1;
                continue;
            }
            start = start.min(range_start);
            end = end.max(range_end);
            self.ranges.remove(idx);
        }
        self.ranges.insert(idx, (start, end));
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct RangeSpan {
    pub(super) first: Option<(u64, u64)>,
    pub(super) last: Option<(u64, u64)>,
    pub(super) ranges: Vec<(u64, u64)>,
}

impl RangeSpan {
    pub(super) fn record(&mut self, range: Option<(u64, u64)>) {
        let Some(range) = range else {
            return;
        };
        if self.first.is_none() {
            self.first = Some(range);
        }
        self.last = Some(range);
        if let Some((_, last_end)) = self.ranges.last_mut()
            && range.0 <= *last_end
        {
            *last_end = (*last_end).max(range.1);
            return;
        }
        self.ranges.push(range);
    }

    pub(super) fn extend(&mut self, other: &RangeSpan) {
        for range in &other.ranges {
            self.record(Some(*range));
        }
    }
}
