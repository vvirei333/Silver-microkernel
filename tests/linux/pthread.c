#include <errno.h>
#include <pthread.h>
#include <stdio.h>

static int value;

static void *worker(void *argument) {
    (void)argument;
    value = 42;
    return 0;
}

int main(void) {
    pthread_t thread;
    int result = pthread_create(&thread, 0, worker, 0);
    if (result != 0) {
        printf("pthread_create %d errno %d\n", result, errno);
        return 2;
    }
    if (pthread_join(thread, 0) != 0) {
        return 3;
    }
    printf("pthread %d\n", value);
    return value == 42 ? 0 : 1;
}
