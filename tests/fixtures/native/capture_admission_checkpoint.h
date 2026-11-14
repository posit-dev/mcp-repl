#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static _Thread_local pthread_mutex_t *locks[64];
static _Thread_local size_t lock_count;
static _Thread_local pthread_mutex_t *attachment;
static atomic_bool armed;
static atomic_bool selected;

static bool contains(const void *buffer, size_t length, const char *text) {
    size_t size = strlen(text);
    for (size_t offset = 0; offset + size <= length; ++offset)
        if (memcmp((const char *)buffer + offset, text, size) == 0) return true;
    return false;
}

static int observed_lock(pthread_mutex_t *mutex) {
    int result = pthread_mutex_lock(mutex);
    if (result == 0) {
        if (lock_count == 64) _exit(130);
        locks[lock_count++] = mutex;
        if (atomic_load(&armed) && lock_count == 4 &&
            !atomic_exchange(&selected, true)) {
            // Prelude adoption holds worker, operation, evaluation, and output
            // mutexes, in that order. Select the operation on its producer,
            // without requiring a recorded attachment event.
            attachment = locks[1];
        }
    }
    return result;
}

static void unlocked(pthread_mutex_t *mutex) {
    if (lock_count != 0 && locks[lock_count - 1] == mutex) --lock_count;
}

static ssize_t observed_read(int descriptor, void *buffer, size_t length) {
    ssize_t result = read(descriptor, buffer, length);
    // The server reads a call from MCP input before it dispatches the handler.
    // The recording writer runs on its own thread and cannot order this gate.
    if (descriptor == STDIN_FILENO && result > 0 &&
        contains(buffer, (size_t)result, getenv("MCP_CONSOLE_TEST_ADMISSION_SOURCE")))
        atomic_store(&armed, true);
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
INTERPOSE(observed_read, read)
