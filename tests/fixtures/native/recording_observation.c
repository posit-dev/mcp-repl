#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static _Atomic int observed;
static _Atomic int close_observed;
static pid_t owner;

__attribute__((constructor)) static void initialize(void) {
    owner = getpid();
    unsetenv("DYLD_INSERT_LIBRARIES");
    unsetenv("LD_PRELOAD");
}

// Text and event markers also match journal lines that quote the same bytes.
// A path suffix selects writes to one recording file, whatever their content.
static int descriptor_path_ends_with(int descriptor, const char *suffix) {
    char path[PATH_MAX];
#ifdef __APPLE__
    if (fcntl(descriptor, F_GETPATH, path) != 0) return 0;
#else
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", descriptor);
    ssize_t size = readlink(link, path, sizeof(path) - 1);
    if (size < 0) return 0;
    path[size] = '\0';
#endif
    size_t length = strlen(path), tail = strlen(suffix);
    return length >= tail && strcmp(path + length - tail, suffix) == 0;
}

// Report the checkpoint, then wait for its release when one is configured.
static void hold(const char *reached_path, const char *release_path) {
    int reached = open(reached_path, O_WRONLY | O_NONBLOCK);
    if (reached < 0 || write(reached, "1", 1) != 1) _exit(124);
    close(reached);
    if (release_path != NULL) {
        int release = open(release_path, O_RDONLY);
        if (release < 0) _exit(125);
        char token;
        ssize_t count;
        do { count = read(release, &token, 1); } while (count < 0 && errno == EINTR);
        if (count != 1 || token != '1') _exit(126);
        close(release);
    }
}

static ssize_t observed_write(int descriptor, const void *buffer, size_t length) {
#ifdef __APPLE__
    ssize_t result = write(descriptor, buffer, length);
#else
    ssize_t (*next_write)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    ssize_t result = next_write(descriptor, buffer, length);
#endif
    const char *event = getenv("MCP_CONSOLE_TEST_OBSERVED_EVENT");
    const char *text = getenv("MCP_CONSOLE_TEST_OBSERVED_TEXT");
    const char *file = getenv("MCP_CONSOLE_TEST_OBSERVED_FILE");
    const char *path = getenv("MCP_CONSOLE_TEST_JOURNAL_OBSERVED");
    const char *delay = getenv("MCP_CONSOLE_TEST_WRITE_DELAY_MS");
    if (getpid() == owner && (event != NULL || text != NULL || file != NULL) && (path != NULL || delay != NULL) && result == (ssize_t)length) {
        char marker[128];
        int size = text != NULL ? snprintf(marker, sizeof(marker), "%s", text)
                               : snprintf(marker, sizeof(marker), "{\"event\":\"%s\",", event);
        int matches = 0;
        if (file != NULL) {
            matches = descriptor_path_ends_with(descriptor, file);
        } else if (size > 0 && (size_t)size < sizeof(marker) && length >= (size_t)size) {
            size_t last = text != NULL ? length - (size_t)size : 0;
            for (size_t offset = 0; offset <= last; offset++) {
                if (memcmp((const char *)buffer + offset, marker, size) == 0) {
                    matches = 1;
                    break;
                }
            }
        }
        if (matches && delay != NULL) {
            // A slow write: the first match takes this long and still succeeds.
            if (!atomic_exchange(&observed, 1)) {
                long milliseconds = atol(delay);
                struct timespec pause = {milliseconds / 1000, milliseconds % 1000 * 1000000};
                while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {}
            }
        } else if (matches && !atomic_exchange(&observed, 1)) {
            hold(path, getenv("MCP_CONSOLE_TEST_JOURNAL_RELEASE"));
        }
    }
    return result;
}

// Hold the first close of the recording file whose path ends with
// MCP_CONSOLE_TEST_HELD_CLOSE, on whichever thread closes it.
static int observed_close(int descriptor) {
    const char *file = getenv("MCP_CONSOLE_TEST_HELD_CLOSE");
    if (file != NULL && getpid() == owner && descriptor_path_ends_with(descriptor, file) &&
        !atomic_exchange(&close_observed, 1)) {
        hold(getenv("MCP_CONSOLE_TEST_CLOSE_REACHED"), getenv("MCP_CONSOLE_TEST_CLOSE_RELEASE"));
    }
#ifdef __APPLE__
    return close(descriptor);
#else
    int (*next_close)(int) = dlsym(RTLD_NEXT, "close");
    return next_close(descriptor);
#endif
}

#ifdef __APPLE__
__attribute__((used)) static struct {
    const void *replacement;
    const void *replacee;
} interpose_write __attribute__((section("__DATA,__interpose"))) = {
    (const void *)(uintptr_t)&observed_write, (const void *)(uintptr_t)&write,
}, interpose_close __attribute__((section("__DATA,__interpose"))) = {
    (const void *)(uintptr_t)&observed_close, (const void *)(uintptr_t)&close,
};
#else
ssize_t write(int descriptor, const void *buffer, size_t length) {
    return observed_write(descriptor, buffer, length);
}
int close(int descriptor) {
    return observed_close(descriptor);
}
#endif
