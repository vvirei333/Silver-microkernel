/* Signal dispositions are per thread group, not per thread.
 *
 *   linuxrun /sfs/linux-groupsig
 *
 * A worker installs a SIGUSR1 handler and joins; the main thread then raises
 * SIGUSR1. With shared dispositions the handler runs and prints "group
 * signal ok"; with per-task tables the default action would kill the main
 * thread instead.
 */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>

static volatile sig_atomic_t got;

static void handler(int signal) {
    (void)signal;
    got = 1;
}

static void *worker(void *argument) {
    (void)argument;
    signal(SIGUSR1, handler);
    return 0;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0) {
        return 2;
    }
    if (pthread_join(thread, 0) != 0) {
        return 3;
    }
    raise(SIGUSR1);
    if (!got) {
        printf("disposition not shared\n");
        return 1;
    }
    printf("group signal ok\n");
    return 0;
}
