/* epoll + timerfd + eventfd event loop.
 *
 *   linuxrun /sfs/linux-eventloop
 *
 * Expected: "event loop ok" and exit 0.
 */
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/timerfd.h>
#include <unistd.h>

int main(void) {
    int epoll = epoll_create1(0);
    if (epoll < 0) {
        return 2;
    }
    int pipefd[2];
    if (pipe(pipefd) != 0) {
        return 3;
    }
    int event = eventfd(0, EFD_NONBLOCK);
    if (event < 0) {
        return 4;
    }
    int timer = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    if (timer < 0) {
        return 5;
    }

    struct epoll_event interest = {.events = EPOLLIN, .data.u64 = 0x1111};
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, pipefd[0], &interest) != 0) {
        return 6;
    }
    interest.data.u64 = 0x2222;
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, event, &interest) != 0) {
        return 7;
    }
    interest.data.u64 = 0x3333;
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, timer, &interest) != 0) {
        return 8;
    }

    struct epoll_event ready[4];
    if (epoll_wait(epoll, ready, 4, 20) != 0) {
        return 9;
    }

    if (write(pipefd[1], "x", 1) != 1) {
        return 10;
    }
    int count = epoll_wait(epoll, ready, 4, 1000);
    if (count != 1 || ready[0].data.u64 != 0x1111) {
        return 11;
    }
    char byte;
    if (read(pipefd[0], &byte, 1) != 1) {
        return 12;
    }

    struct itimerspec spec;
    spec.it_interval.tv_sec = 0;
    spec.it_interval.tv_nsec = 0;
    spec.it_value.tv_sec = 0;
    spec.it_value.tv_nsec = 50 * 1000 * 1000;
    if (timerfd_settime(timer, 0, &spec, 0) != 0) {
        return 13;
    }
    count = epoll_wait(epoll, ready, 4, 1000);
    if (count != 1 || ready[0].data.u64 != 0x3333) {
        return 14;
    }
    uint64_t expirations = 0;
    if (read(timer, &expirations, sizeof(expirations)) != 8 || expirations < 1) {
        return 15;
    }

    uint64_t one = 1;
    if (write(event, &one, sizeof(one)) != 8) {
        return 16;
    }
    count = epoll_wait(epoll, ready, 4, 1000);
    if (count != 1 || ready[0].data.u64 != 0x2222) {
        return 17;
    }
    if (read(event, &expirations, sizeof(expirations)) != 8) {
        return 18;
    }

    if (epoll_ctl(epoll, EPOLL_CTL_DEL, pipefd[0], 0) != 0) {
        return 19;
    }
    if (epoll_wait(epoll, ready, 4, 20) != 0) {
        return 20;
    }

    struct itimerspec zero;
    zero.it_interval.tv_sec = 0;
    zero.it_interval.tv_nsec = 0;
    zero.it_value.tv_sec = 0;
    zero.it_value.tv_nsec = 0;
    if (timerfd_settime(timer, 0, &zero, 0) != 0) {
        return 21;
    }
    if (timerfd_gettime(timer, &spec) != 0) {
        return 22;
    }
    if (spec.it_value.tv_sec != 0 || spec.it_value.tv_nsec != 0) {
        return 23;
    }

    printf("event loop ok\n");
    return 0;
}
