/* Publish a headless Wayland compositor's output into a file on the SFS.
 *
 * The Wayland client is hand-rolled on raw syscalls (no libwayland): connect
 * to the compositor socket, get_registry, bind wl_shm/wl_output and
 * zwlr_screencopy_manager_v1, capture the output, create a wl_shm pool with
 * an fd passed over SCM_RIGHTS, copy the frame and convert it to RGBA8888.
 * The host reads /frame.rgba straight out of the disk image, so no ivshmem
 * device is required on the QEMU build.
 *
 * Wire format reminders: message header is two words, object id and
 * (size << 16) | opcode; registry binds allocate ids from the client range
 * (we start at 0xFF000000); the compositor assigns server-side ids.
 *
 *   WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/tmp screencopy-minimal
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <unistd.h>

#define DISPLAY_ID 1
#define REGISTRY_ID 2
#define SYNC_ID 3
#define SHM_ID 4
#define OUTPUT_ID 5
#define MANAGER_ID 6
#define FRAME_ID 7
#define POOL_ID 8
#define BUFFER_ID 9
#define CALLBACK_ID 10

/* wl_registry.bind = opcode 1; wl_display.sync = 1. */
#define WL_DISPLAY_SYNC 1
#define WL_REGISTRY_BIND 1
#define WL_SHM_CREATE_POOL 0
#define WL_SHM_POOL_CREATE_BUFFER 1
#define MANAGER_CAPTURE_OUTPUT 0
#define MANAGER_DESTROY 2
#define FRAME_COPY 0
#define FRAME_DESTROY 1

/* wl_shm formats we accept (from the wl_shm enum). */
#define WL_SHM_FORMAT_ARGB8888 0
#define WL_SHM_FORMAT_XRGB8888 1

#define SCANOUT_PATH "/frame.rgba"

static int display_fd = -1;
static int have_shm, have_output, have_manager;
static uint32_t shm_name, output_name, manager_name;
static uint32_t shm_version, output_version, manager_version;
static uint32_t frame_format, frame_width, frame_height, frame_stride;
static int have_buffer_info, frame_ready, frame_failed;
static uint8_t *pixels;
static size_t pixels_size;

/* Message ids the compositor assigns for server-created objects. */
static uint32_t server_buffer_id, server_pool_id;

static int put_u32(uint8_t **cursor, uint32_t value) {
    memcpy(*cursor, &value, 4);
    *cursor += 4;
    return 4;
}

static int put_string(uint8_t **cursor, const char *text) {
    uint32_t length = (uint32_t)strlen(text) + 1;
    put_u32(cursor, length);
    memcpy(*cursor, text, length);
    *cursor += length;
    uint32_t pad = (4 - (length & 3)) & 3;
    memset(*cursor, 0, pad);
    *cursor += pad;
    return (int)(4 + length + pad);
}

static void display_write(uint8_t *buffer, size_t length) {
    size_t done = 0;
    while (done < length) {
        ssize_t written = write(display_fd, buffer + done, length - done);
        if (written <= 0) {
            if (written < 0 && errno == EINTR) continue;
            perror("write wayland");
            exit(1);
        }
        done += (size_t)written;
    }
}

/* Sends one request with its fixed-size arguments; strings need the caller
 * to build the buffer (see bind_global). */
static void send_simple(uint32_t object, uint32_t opcode, size_t words,
                        const uint32_t *args) {
    uint8_t buffer[256];
    size_t length = 8 + words * 4;
    uint32_t header[2] = {object, (uint32_t)((length << 16) | opcode)};
    memcpy(buffer, header, 8);
    if (words) memcpy(buffer + 8, args, words * 4);
    display_write(buffer, length);
}

static void bind_global(uint32_t new_id, uint32_t name, const char *interface,
                        uint32_t version) {
    uint8_t buffer[128];
    uint8_t *cursor = buffer + 8;
    uint32_t header[2];
    put_u32(&cursor, name);
    put_string(&cursor, interface);
    put_u32(&cursor, version);
    put_u32(&cursor, new_id); /* new_id is the last wire argument */
    size_t length = (size_t)(cursor - buffer);
    header[0] = REGISTRY_ID;
    header[1] = (uint32_t)((length << 16) | WL_REGISTRY_BIND);
    memcpy(buffer, header, 8);
    display_write(buffer, length);
}

/* Reads one message; returns -1 on protocol error/EOF. Arguments beyond the
 * first two words land in out_args (up to 64 words). */
static int display_read(uint32_t *object, uint32_t *opcode, uint32_t *out_args,
                        size_t *out_words, int *out_fd) {
    static uint8_t stash[65536];
    static size_t stash_size;
    static int eof_retries;
    for (;;) {
        while (stash_size >= 8) {
            uint32_t id, second;
            memcpy(&id, stash, 4);
            memcpy(&second, stash + 4, 4);
            uint32_t size = second >> 16;
            uint32_t message_opcode = second & 0xFFFF;
            if (size < 8 || stash_size < size) break;
            *object = id;
            *opcode = message_opcode;
            size_t arg_words = (size - 8) / 4;
            if (arg_words > 64) arg_words = 64;
            memcpy(out_args, stash + 8, arg_words * 4);
            *out_words = arg_words;
            *out_fd = -1;
            memmove(stash, stash + size, stash_size - size);
            stash_size -= size;
            return (int)size;
        }
        ssize_t got = read(display_fd, stash + stash_size,
                           sizeof(stash) - stash_size);
        if (got < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        if (got == 0) {
            /* A spuriously-empty read right after connect: retry before
             * believing the compositor closed the connection. */
            eof_retries++;
            if (eof_retries > 5) {
                fprintf(stderr, "sc: eof after %d retries\n", eof_retries);
                return -1;
            }
            sleep(1);
            continue;
        }
        if (got < 0) {
            fprintf(stderr, "sc: read errno=%d\n", errno);
            return -1;
        }
        eof_retries = 0;
        stash_size += (size_t)got;
    }
}

/* Receives one fd through SCM_RIGHTS (for wl_shm.create_pool the fd travels
 * with the REQUEST, so this helper is used on the send side instead). */
static int send_with_fd(uint32_t object, uint32_t opcode, size_t words,
                        const uint32_t *args, int fd) {
    uint8_t buffer[256];
    size_t length = 8 + words * 4;
    uint32_t header[2] = {object, (uint32_t)((length << 16) | opcode)};
    memcpy(buffer, header, 8);
    if (words) memcpy(buffer + 8, args, words * 4);

    struct msghdr message;
    memset(&message, 0, sizeof(message));
    struct iovec iov = {buffer, length};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    char control[CMSG_SPACE(sizeof(int))];
    message.msg_control = control;
    message.msg_controllen = sizeof(control);
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));

    ssize_t sent = sendmsg(display_fd, &message, 0);
    return sent == (ssize_t)length ? 0 : -1;
}

static void registry_scan(const char *buffer, size_t length) {
    /* Every global: name u32, interface string, version u32. */
    size_t offset = 0;
    while (offset + 8 <= length) {
        uint32_t name;
        memcpy(&name, buffer + offset, 4);
        uint32_t string_length;
        memcpy(&string_length, buffer + offset + 4, 4);
        if (string_length == 0 || offset + 8 + string_length > length) break;
        const char *interface = buffer + offset + 8;
        uint32_t padded = (string_length + 3) & ~3u;
        size_t version_offset = offset + 8 + padded;
        uint32_t version;
        memcpy(&version, buffer + version_offset, 4);
        offset = version_offset + 4;

        if (strcmp(interface, "wl_shm") == 0) {
            shm_name = name; shm_version = version; have_shm = 1;
        } else if (strcmp(interface, "wl_output") == 0 && !have_output) {
            output_name = name; output_version = version; have_output = 1;
        } else if (strcmp(interface, "zwlr_screencopy_manager_v1") == 0) {
            manager_name = name;
            manager_version = version < 3 ? version : 3;
            have_manager = 1;
        }
    }
}

int main(void) {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    const char *name = getenv("WAYLAND_DISPLAY");
    if (!runtime) runtime = "/tmp";
    if (!name) name = "wayland-0";

    char path[128];
    snprintf(path, sizeof(path), "%s/%s", runtime, name);
    display_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (display_fd < 0) { perror("socket"); return 1; }
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    strncpy(address.sun_path, path, sizeof(address.sun_path) - 1);
    if (connect(display_fd, (struct sockaddr *)&address, sizeof(address)) < 0) {
        perror("connect wayland");
        return 1;
    }

    /* get_registry: the new_id (REGISTRY_ID) is the message body */
    uint32_t get_registry[3] = {DISPLAY_ID, (3u << 16) | 1u, REGISTRY_ID};
    display_write((uint8_t *)get_registry, 12);

    uint32_t args[64];
    size_t words;
    uint32_t object, opcode;
    int received_sync = 0;
    while (!received_sync) {
        uint32_t message_size;
        if (display_read(&object, &opcode, args, &words, NULL) < 0) {
            fprintf(stderr, "sc: display read failed\n");
            return 1;
        }
        message_size = words * 4;
        if (object == DISPLAY_ID && opcode == 0) { /* wl_display.error */
            fprintf(stderr, "sc: display error: %.60s\n",
                    (const char *)&args[2]);
            return 1;
        }
        if (object == REGISTRY_ID && opcode == 0) { /* global */
            registry_scan((const char *)args, message_size);
        } else if (object == REGISTRY_ID && opcode == 1) {
            /* global_remove: ignored */
        } else if (object == DISPLAY_ID && opcode == 1 && args[0] == SYNC_ID) {
            received_sync = 1; /* wl_callback.done */
        }
    }
    fprintf(stderr, "sc: globals shm=%u output=%u manager=%u\n",
            have_shm, have_output, have_manager);
    if (!have_shm || !have_output || !have_manager) {
        fprintf(stderr, "sc: missing globals\n");
        return 1;
    }

    bind_global(SHM_ID, shm_name, "wl_shm", 1);
    bind_global(OUTPUT_ID, output_name, "wl_output", 1);
    bind_global(MANAGER_ID, manager_name, "zwlr_screencopy_manager_v1",
                manager_version);

    /* capture_output(0): new_id frame, overlay_cursor i, output o. */
    uint32_t capture[3] = {FRAME_ID, 0, OUTPUT_ID};
    send_simple(MANAGER_ID, MANAGER_CAPTURE_OUTPUT, 3, capture);

    /* Wait for the buffer event, then create the pool/buffer. */
    int have_pool = 0;
    int memfd = -1;
    while (!have_pool && !frame_failed) {
        if (display_read(&object, &opcode, args, &words, NULL) < 0) {
            fprintf(stderr, "sc: read failed during buffer setup\n");
            return 1;
        }
        if (object == FRAME_ID && opcode == 0) { /* buffer: new wl_buffer id */
            server_buffer_id = args[0];
        } else if (object == FRAME_ID && opcode == 2) { /* ready */
            frame_ready = 1;
        } else if (object == FRAME_ID && opcode == 3) { /* failed */
            frame_failed = 1;
        } else if (object == FRAME_ID && opcode == 6) { /* buffer_done (v3) */
            if (!server_buffer_id || frame_width == 0) {
                fprintf(stderr, "sc: buffer_done without buffer info\n");
                return 1;
            }
            size_t size = (size_t)frame_stride * frame_height;
            memfd = (int)syscall(SYS_memfd_create, "screencopy", 0);
            if (memfd < 0) { perror("memfd_create"); return 1; }
            if (ftruncate(memfd, (off_t)size) != 0) { perror("ftruncate"); return 1; }
            pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, memfd, 0);
            if (pixels == MAP_FAILED) { perror("mmap"); return 1; }
            pixels_size = size;
            server_pool_id = POOL_ID;
            /* wl_shm.create_pool(0): new_id pool, fd (SCM_RIGHTS), size i */
            uint32_t pool_args[2] = {POOL_ID, (uint32_t)size};
            if (send_with_fd(SHM_ID, WL_SHM_CREATE_POOL, 2, pool_args, memfd) < 0) {
                perror("sendmsg pool");
                return 1;
            }
            /* wl_shm_pool.create_buffer(1): new_id buffer, offset i, w i,
             * h i, stride i, format u. */
            uint32_t buffer_args[6] = {BUFFER_ID, 0, frame_width, frame_height,
                                       frame_stride, frame_format};
            send_simple(server_pool_id, WL_SHM_POOL_CREATE_BUFFER, 6, buffer_args);
            have_pool = 1;
            close(memfd);
        } else if (object == FRAME_ID && opcode == 1) { /* flags */
            /* y-invert etc.: ignored, pixman formats are straight */
        } else if (object == FRAME_ID && opcode == 4) { /* damage */
            /* ignored */
        } else if (object == FRAME_ID && opcode == 5) { /* linux_dmabuf: fd */
            close((int)args[0]);
        } else if (object == DISPLAY_ID && opcode == 0) {
            fprintf(stderr, "sc: display error: %.60s\n",
                    (const char *)&args[2]);
            return 1;
        }
        /* wl_output geometry/mode events land on OUTPUT_ID and are ignored */
    }
    if (frame_failed) {
        fprintf(stderr, "sc: frame failed\n");
        return 1;
    }

    /* frame.copy(0): object buffer. */
    uint32_t copy_args[1] = {BUFFER_ID};
    send_simple(FRAME_ID, FRAME_COPY, 1, copy_args);

    frame_ready = 0;
    while (!frame_ready && !frame_failed) {
        if (display_read(&object, &opcode, args, &words, NULL) < 0) {
            fprintf(stderr, "sc: read failed during copy\n");
            return 1;
        }
        if (object == FRAME_ID && opcode == 2) frame_ready = 1;
        else if (object == FRAME_ID && opcode == 3) frame_failed = 1;
        else if (object == FRAME_ID && opcode == 0) server_buffer_id = args[0];
        else if (object == DISPLAY_ID && opcode == 0) {
            fprintf(stderr, "sc: display error: %.60s\n",
                    (const char *)&args[2]);
            return 1;
        }
    }
    if (frame_failed) {
        fprintf(stderr, "sc: copy failed\n");
        return 1;
    }

    int fd = open(SCANOUT_PATH, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open " SCANOUT_PATH); return 1; }
    uint32_t header[6] = {0x57494E31u, 1u, frame_width, frame_height,
                          frame_width * 4, frame_format};
    if (write(fd, header, sizeof(header)) != (ssize_t)sizeof(header)) {
        perror("write header"); return 1;
    }
    int swap = frame_format == WL_SHM_FORMAT_ARGB8888 ||
               frame_format == WL_SHM_FORMAT_XRGB8888;
    uint8_t row[4096 * 4];
    for (uint32_t y = 0; y < frame_height; y++) {
        const uint8_t *src = pixels + (size_t)y * frame_stride;
        if (!swap) {
            if (write(fd, src, (size_t)frame_width * 4) !=
                (ssize_t)(frame_width * 4)) { perror("write row"); return 1; }
        } else {
            for (uint32_t x = 0; x < frame_width; x++) {
                row[x * 4 + 0] = src[x * 4 + 2];
                row[x * 4 + 1] = src[x * 4 + 1];
                row[x * 4 + 2] = src[x * 4 + 0];
                row[x * 4 + 3] = src[x * 4 + 3];
            }
            if (write(fd, row, (size_t)frame_width * 4) !=
                (ssize_t)(frame_width * 4)) { perror("write row"); return 1; }
        }
    }
    fsync(fd);
    close(fd);
    printf("SCREENCOPY FRAME %ux%u format=%u\n", frame_width, frame_height,
           frame_format);
    fflush(stdout);
    return 0;
}
