#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

static void *worker(void *argument) {
    (void)argument;
    sleep(5);
    return 0;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0) {
        return 2;
    }
    printf("exit group ok\n");
    return 0;
}
