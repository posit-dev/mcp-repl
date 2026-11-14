#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static _Thread_local pthread_mutex_t *locks[64];
static _Thread_local size_t lock_count;
static atomic_bool attaching;
static atomic_bool interrupt_received;
static atomic_bool paused;
static atomic_uintptr_t operation;

__attribute__((constructor)) static void initialize(void) {
    // Observe only the controller; the worker keeps ordinary input handling.
    if (unsetenv("DYLD_INSERT_LIBRARIES") != 0) _exit(120);
}

static bool contains(const void *buffer, size_t length, const char *text) {
    size_t size = strlen(text);
    for (size_t offset = 0; offset + size <= length; ++offset)
        if (memcmp((const char *)buffer + offset, text, size) == 0) return true;
    return false;
}

// The recording writer journals on its own thread and cannot order these
// gates. Observe each request as the server receives it, before its handler.
static void received(const void *buffer, ssize_t length) {
    if (length <= 0) return;
    if (contains(buffer, (size_t)length, "selected interrupt target: "))
        atomic_store(&attaching, true);
    if (contains(buffer, (size_t)length, "\"interrupt\""))
        atomic_store(&interrupt_received, true);
}

static ssize_t observed_read(int descriptor, void *buffer, size_t length) {
    ssize_t result = read(descriptor, buffer, length);
    if (descriptor == STDIN_FILENO) received(buffer, result);
    return result;
}

static ssize_t observed_recv(int descriptor, void *buffer, size_t length, int flags) {
    ssize_t result = recv(descriptor, buffer, length, flags);
    received(buffer, result);
    return result;
}

static int observed_lock(pthread_mutex_t *mutex) {
    if (lock_count == 0 && atomic_load(&interrupt_received) &&
        (uintptr_t)mutex == atomic_load(&operation) &&
        !atomic_exchange(&paused, true)) {
        // Selection takes this mutex under publication and lifecycle guards.
        // Its blocking-pool continuation takes it alone, immediately before
        // enqueueing the interrupt. Hold that continuation without any lock.
        int reached = open(getenv("MCP_CONSOLE_TEST_INTERRUPT_SELECTED"), O_WRONLY);
        int release = open(getenv("MCP_CONSOLE_TEST_INTERRUPT_RELEASE"), O_RDONLY);
        if (reached < 0 || release < 0 || write(reached, "1", 1) != 1) _exit(121);
        close(reached);
        char token;
        ssize_t count;
        do { count = read(release, &token, 1); } while (count < 0 && errno == EINTR);
        if (count != 1 || token != '1') _exit(122);
        close(release);
    }
    int result = pthread_mutex_lock(mutex);
    if (result == 0) {
        if (lock_count == 64) _exit(123);
        locks[lock_count++] = mutex;
        if (atomic_load(&attaching) && lock_count == 4 &&
            atomic_load(&operation) == 0) {
            // Prelude adoption holds worker, operation, evaluation, and output
            // mutexes, as in cell_attachment_checkpoint.c.
            atomic_store(&operation, (uintptr_t)locks[1]);
            atomic_store(&attaching, false);
        }
    }
    return result;
}

static int observed_unlock(pthread_mutex_t *mutex) {
    int result = pthread_mutex_unlock(mutex);
    if (result == 0 && lock_count != 0 && locks[lock_count - 1] == mutex)
        --lock_count;
    return result;
}

#define INTERPOSE(replacement, original)                                      \
    __attribute__((used)) static struct {                                     \
        const void *replacement;                                             \
        const void *original;                                                \
    } interpose_##original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)(uintptr_t)&replacement,                                \
        (const void *)(uintptr_t)&original,                                   \
    };
INTERPOSE(observed_lock, pthread_mutex_lock)
INTERPOSE(observed_unlock, pthread_mutex_unlock)
INTERPOSE(observed_read, read)
INTERPOSE(observed_recv, recv)
