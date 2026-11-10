/* Futex ping-pong in the shape event-listener's `parking` uses: a shared
 * word, FUTEX_WAIT when it has the expected value, FUTEX_WAKE after a store.
 * A lost wakeup shows up as a hang.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile int *word;

static long futex(volatile int *address, int operation, int value) {
    register long x8 __asm__("x8") = 98;
    register long x0 __asm__("x0") = (long)address;
    register long x1 __asm__("x1") = operation;
    register long x2 __asm__("x2") = value;
    register long x3 __asm__("x3") = 0;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
}

#define ROUNDS 20000

static void *worker(void *argument) {
    (void)argument;
    int seen = 0;
    while (seen < ROUNDS) {
        int value = *word;
        if (value != seen) {
            seen = value;
            continue;
        }
        /* Wait for the counter to change from `seen`. */
        futex(word, 0 /* FUTEX_WAIT */, seen);
    }
    return NULL;
}

int main(void) {
    word = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (word == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 2;
    }
    for (int round = 1; round <= ROUNDS; round++) {
        *word = round;
        __sync_synchronize();
        futex(word, 1 /* FUTEX_WAKE */, 1);
        if ((round % 5000) == 0) {
            printf("futex rounds=%d\n", round);
            fflush(stdout);
        }
    }
    pthread_join(thread, NULL);
    printf("futex pingpong ok %d\n", ROUNDS);
    fflush(stdout);
    return 0;
}
