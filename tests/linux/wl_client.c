/* Minimal Wayland CLIENT against deniald's wayland-1 socket.
 *
 *   linuxrun /sfs/wl-client
 *
 * Connects to /tmp/wayland-1, binds wl_compositor + wl_shm, fills a 128x128
 * XRGB shm buffer with a fixed pattern, attaches/damages/commits it, then
 * syncs and exits 0. Used to check whether deniald's compositor issues a
 * real atomic commit for an external client (the frame-clock investigation).
 */
#include "wl_client_wire.h"

static const char SOCKET_PATH[] = "/tmp/wayland-1";

static unsigned registry_id = 2;
static unsigned sync_id = 3;
static unsigned compositor_id;
static unsigned shm_id;
static unsigned pool_id = 6;
static unsigned buffer_id = 7;
static unsigned surface_id = 8;
static unsigned sync2_id = 9;

static void emit(const char *text, unsigned length) {
    sys6(64, 1, (long)text, length, 0, 0, 0);
}

static void line(const char *text) {
    unsigned length = 0;
    while (text[length]) length++;
    emit(text, length);
    emit("\n", 1);
}

static void line_hex(const char *prefix, unsigned value) {
    char buffer[40];
    unsigned at = 0;
    while (prefix[at]) { buffer[at] = prefix[at]; at++; }
    buffer[at++] = ' ';
    for (int shift = 28; shift >= 0; shift -= 4) {
        unsigned digit = (value >> shift) & 0xf;
        buffer[at++] = digit < 10 ? (char)('0' + digit) : (char)('a' + digit - 10);
    }
    buffer[at++] = '\n';
    emit(buffer, at);
}

static int attempts;
static void retry_sleep(void) {
    struct { long seconds; long nanoseconds; } spec = { 3, 0 };
    sys6(115 /* clock_nanosleep */, 1 /* CLOCK_MONOTONIC */, 0, (long)&spec, 0, 0, 0);
}

void _start(void) {
    if (attempts > 0) {
        line("wl-client: retrying");
        retry_sleep();
        if (attempts > 25) {
            line("wl-client: giving up");
            sys6(93, 1, 0, 0, 0, 0, 0);
        }
    }
    attempts++;
    line("wl-client: connecting");
    long socket = sys6(198, 1 /* AF_UNIX */, 2 /* SOCK_STREAM */, 0, 0, 0, 0);
    if (socket < 0) { line("socket failed"); sys6(93, 1, 0, 0, 0, 0, 0); }
    if (connect_path(socket, SOCKET_PATH) < 0) {
        line("connect failed");
        retry_sleep();
        sys6(93, 1, 0, 0, 0, 0, 0); // unreachable; main() loop handles retries
    }
    line("connected");

    // Build the shared buffer before the handshake: memfd 128*128*4.
    long size = 128 * 128 * 4;
    long memfd = sys6(279 /* memfd_create, aarch64 */, (long)"wl-client", 0, 0, 0, 0, 0);
    line_hex("memfd", (unsigned)memfd);
    if (memfd < 0) sys6(93, 1, 0, 0, 0, 0, 0);
    static unsigned char pixels[128 * 128 * 4];
    for (unsigned row = 0; row < 128; row++) {
        for (unsigned column = 0; column < 128; column++) {
            unsigned offset = (row * 128 + column) * 4;
            // XRGB8888 little-endian bytes: B, G, R, X.
            pixels[offset] = 0x00;
            pixels[offset + 1] = 0x40;
            pixels[offset + 2] = 0xC8;
            pixels[offset + 3] = 0x00;
            if ((row / 16 + column / 16) % 2 == 0) {
                pixels[offset] = 0xE0;
                pixels[offset + 1] = 0xE0;
                pixels[offset + 2] = 0x20;
            }
        }
    }
    long wrote = sys6(64, memfd, (long)pixels, size, 0, 0, 0);
    line_hex("wrote", (unsigned)wrote);

    begin(1 /* wl_display */, 1 /* get_registry */);
    put_new_id(registry_id);
    finish();
    send_message(socket, -1);
    begin(1, 0 /* wl_display.sync */);
    put_new_id(sync_id);
    finish();
    send_message(socket, -1);

    unsigned compositor_name = 0;
    unsigned shm_name = 0;
    int got_done = 0;
    for (int turn = 0; turn < 200 && !got_done; turn++) {
        long got = receive_message(socket);
        if (got == -2) { continue; }
        if (got <= 0) {
            line("registry read failed");
            line_hex("recv", (unsigned)got);
            sys3(93, 0, 0, 0); // retry the whole handshake from _start
        }
        while (parse_next_message()) {
            unsigned object = object_of();
            unsigned opcode = opcode_of();
            if (object == registry_id && opcode == 0 /* global */) {
                unsigned name = take_u32();
                char interface[64];
                take_string(interface, sizeof(interface));
                unsigned version = take_u32();
                if (text_equal(interface, "wl_compositor")) {
                    compositor_name = name;
                    compositor_version = version;
                } else if (text_equal(interface, "wl_shm")) {
                    shm_name = name;
                }
            } else if (object == sync_id && opcode == 0 /* done */) {
                got_done = 1;
            } else if (object == 1 && opcode == 0 /* wl_display.error */) {
                line("display error");
                sys6(93, 1, 0, 0, 0, 0, 0);
            }
        }
    }
    line_hex("compositor_name", compositor_name);
    line_hex("shm_name", shm_name);
    if (!compositor_name || !shm_name) {
        line("missing globals");
        sys3(93, 0, 0, 0); // retry
    }

    // bind wl_compositor (version 1) and wl_shm.
    begin(registry_id, 0 /* bind */);
    put_u32(compositor_name);
    put_string("wl_compositor");
    put_u32(1);
    put_new_id(compositor_id = 4);
    finish();
    send_message(socket, -1);
    begin(registry_id, 0);
    put_u32(shm_name);
    put_string("wl_shm");
    put_u32(1);
    put_new_id(shm_id = 5);
    finish();
    send_message(socket, -1);

    begin(shm_id, 0 /* create_pool */);
    put_new_id(pool_id);
    put_i32((int)memfd);
    put_i32((int)size);
    finish();
    send_message(socket, memfd);

    begin(pool_id, 0 /* create_buffer */);
    put_new_id(buffer_id);
    put_i32(0);
    put_i32(128);
    put_i32(128);
    put_i32(128 * 4);
    put_u32(1 /* XRGB8888 */);
    finish();
    send_message(socket, -1);

    begin(compositor_id, 0 /* create_surface */);
    put_new_id(surface_id);
    finish();
    send_message(socket, -1);

    begin(surface_id, 1 /* attach */);
    put_u32(buffer_id);
    put_i32(0);
    put_i32(0);
    finish();
    send_message(socket, -1);
    begin(surface_id, 2 /* damage */);
    put_i32(0);
    put_i32(0);
    put_i32(128);
    put_i32(128);
    finish();
    send_message(socket, -1);
    begin(surface_id, 3 /* commit */);
    finish();
    send_message(socket, -1);
    line("committed");

    // Let the compositor react, then sync again to prove the connection is
    // still healthy.
    for (int turn = 0; turn < 200; turn++) {
        long got = receive_message(socket);
        if (got == -2) {
            if (turn > 20) break;
            continue;
        }
        if (got <= 0) { line("post-commit read failed"); sys6(93, 1, 0, 0, 0, 0, 0); }
        while (parse_next_message()) {
            unsigned object = object_of();
            unsigned opcode = opcode_of();
            if (object == 1 && opcode == 0) {
                line("display error after commit");
                sys6(93, 1, 0, 0, 0, 0, 0);
            }
        }
    }
    begin(1, 0);
    put_new_id(sync2_id);
    finish();
    send_message(socket, -1);
    int done2 = 0;
    for (int turn = 0; turn < 100 && !done2; turn++) {
        long got = receive_message(socket);
        if (got == -2) continue;
        if (got <= 0) break;
        while (parse_next_message()) {
            if (object_of() == sync2_id && opcode_of() == 0) {
                done2 = 1;
            }
        }
    }
    line_hex("final sync done", (unsigned)done2);
    if (done2) {
        line("wl-client ok");
        sys6(93, 0, 0, 0, 0, 0, 0);
    }
    line("wl-client: no final sync");
    sys6(93, 1, 0, 0, 0, 0, 0);
}
