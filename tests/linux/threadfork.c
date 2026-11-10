/* fork() called from a non-main thread. */
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static void *worker(void *argument) {
    (void)argument;
    printf("thread before fork\n");
    fflush(stdout);
    pid_t child = fork();
    printf("thread after fork p=%d\n", (int)child);
    fflush(stdout);
    if (child == 0) {
        printf("child running\n");
        fflush(stdout);
        _exit(0);
    }
    int status = 0;
    waitpid(child, &status, 0);
    printf("thread wait status=%d\n", status);
    fflush(stdout);
    return NULL;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
        perror("pthread_create");
        return 1;
    }
    pthread_join(thread, NULL);
    printf("main done\n");
    return 0;
}
