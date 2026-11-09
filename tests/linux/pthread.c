#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int value;

static void *worker(void *argument) {
    (void)argument;
    char *buffer = malloc(1024);
    if (buffer == 0) {
        return (void *)1;
    }
    buffer[0] = 'x';
    printf("worker %c\n", buffer[0]);
    free(buffer);
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
    void *returned = 0;
    if (pthread_join(thread, &returned) != 0 || returned != 0) {
        return 3;
    }
    printf("pthread %d\n", value);
    return value == 42 ? 0 : 1;
}
