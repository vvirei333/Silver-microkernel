/* A process leader may exit while its workers keep running.
 *
 *   linuxrun /sfs/linux-pthread-lifetime
 *
 * The main thread calls pthread_exit(NULL) after a worker has signalled that
 * it is inside the process. The worker then sleeps, touches its heap, opens a
 * file and prints before returning. On Linux the mm, descriptor table and
 * signal dispositions belong to the thread group and survive the leader's
 * exit; a kernel that frees the leader's address space immediately faults in
 * the worker or reuses its ASID for another process. The expected output is
 * "worker survived" followed by a clean process exit.
 */
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static volatile int ready;

static void *worker(void *argument) {
    (void)argument;
    char *buffer = malloc(4096);
    if (buffer == 0) {
        printf("worker malloc failed\n");
        return (void *)1;
    }
    buffer[0] = 'o';
    buffer[1] = 'k';
    ready = 1;
    sleep(2);
    /* The leader is gone now. These accesses must still hit the same mm. */
    buffer[2] = '!';
    int fd = open("/sfs/readme.txt", O_RDONLY);
    if (fd >= 0) {
        close(fd);
    }
    printf("worker survived %c%c%c fd=%d\n", buffer[0], buffer[1], buffer[2], fd);
    /* Explicit group exit: the leader is already gone, so returning from the
     * last thread would rely on libc's last-thread path instead of testing the
     * kernel's group lifetime. */
    exit(0);
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0) {
        printf("pthread_create failed\n");
        return 2;
    }
    while (!ready) {
        usleep(1000);
    }
    printf("leader exiting\n");
    pthread_exit(0);
    printf("pthread_exit returned\n");
    return 1;
}
