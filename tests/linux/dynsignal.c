#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static volatile sig_atomic_t got;

static void handler(int signal) {
    got = signal;
}

int main(void) {
    struct sigaction action = {0};
    action.sa_handler = handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, 0) != 0) {
        return 2;
    }
    if (raise(SIGUSR1) != 0) {
        return 3;
    }
    printf("dyn signal %d\n", (int)got);
    return got == SIGUSR1 ? 0 : 1;
}
