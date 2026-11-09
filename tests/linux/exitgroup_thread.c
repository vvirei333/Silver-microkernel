/* exit_group(2) from a worker kills the whole process.
 *
 *   linuxrun /sfs/linux-exitgroup
 *
 * The worker calls exit(0) (glibc uses exit_group) while main sleeps. The
 * process must terminate with status 0 and never reach "main survived"; the
 * fallback in the kernel that exits only the calling thread would let main
 * wake up, print and return 1.
 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void *worker(void *argument) {
    (void)argument;
    printf("worker exiting\n");
    exit(0);
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0) {
        return 2;
    }
    sleep(10);
    printf("main survived\n");
    return 1;
}
