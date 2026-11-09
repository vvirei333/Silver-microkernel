/* glibc AF_UNIX + SCM_RIGHTS + poll + O_NONBLOCK + EOF.
 *
 *   linuxrun /sfs/linux-sockets
 *
 * Expected: "glibc socket ok" and exit 0.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
        perror("socketpair");
        return 2;
    }

    if (write(pair[0], "ping", 4) != 4) {
        return 3;
    }
    char buffer[8];
    if (read(pair[1], buffer, sizeof(buffer)) != 4 || memcmp(buffer, "ping", 4) != 0) {
        return 4;
    }

    int event = eventfd(0, 0);
    if (event < 0) {
        return 5;
    }
    struct iovec vector = {.iov_base = "x", .iov_len = 1};
    char control[CMSG_SPACE(sizeof(int))];
    struct msghdr outgoing;
    memset(&outgoing, 0, sizeof(outgoing));
    outgoing.msg_iov = &vector;
    outgoing.msg_iovlen = 1;
    outgoing.msg_control = control;
    outgoing.msg_controllen = sizeof(control);
    struct cmsghdr *header = CMSG_FIRSTHDR(&outgoing);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &event, sizeof(int));
    if (sendmsg(pair[0], &outgoing, 0) != 1) {
        return 6;
    }

    char payload[8];
    char received[CMSG_SPACE(sizeof(int))];
    struct iovec incoming_vector = {.iov_base = payload, .iov_len = sizeof(payload)};
    struct msghdr incoming;
    memset(&incoming, 0, sizeof(incoming));
    incoming.msg_iov = &incoming_vector;
    incoming.msg_iovlen = 1;
    incoming.msg_control = received;
    incoming.msg_controllen = sizeof(received);
    if (recvmsg(pair[1], &incoming, 0) != 1 || payload[0] != 'x') {
        return 7;
    }
    struct cmsghdr *got = CMSG_FIRSTHDR(&incoming);
    if (got == 0 || got->cmsg_level != SOL_SOCKET || got->cmsg_type != SCM_RIGHTS) {
        return 8;
    }
    int passed;
    memcpy(&passed, CMSG_DATA(got), sizeof(int));

    uint64_t one = 1;
    if (write(passed, &one, sizeof(one)) != sizeof(one)) {
        return 9;
    }
    uint64_t value = 0;
    if (read(event, &value, sizeof(value)) != sizeof(value) || value != 1) {
        return 10;
    }
    close(passed);

    struct pollfd descriptor = {.fd = pair[1], .events = POLLIN};
    if (write(pair[0], "z", 1) != 1) {
        return 11;
    }
    if (poll(&descriptor, 1, 1000) != 1 || (descriptor.revents & POLLIN) == 0) {
        return 12;
    }
    if (read(pair[1], buffer, 1) != 1) {
        return 13;
    }

    int flags = fcntl(pair[1], F_GETFL);
    if (flags < 0 || fcntl(pair[1], F_SETFL, flags | O_NONBLOCK) != 0) {
        return 14;
    }
    errno = 0;
    if (read(pair[1], buffer, 1) != -1 || errno != EAGAIN) {
        return 15;
    }

    if (shutdown(pair[0], SHUT_WR) != 0) {
        return 16;
    }
    if (read(pair[1], buffer, 1) != 0) {
        return 17;
    }

    printf("glibc socket ok\n");
    return 0;
}
