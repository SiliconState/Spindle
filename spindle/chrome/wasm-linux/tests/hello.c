// The wasm Linux's first program: as /init it says hello through /dev/kmsg, starts a
// copy of itself (posix_spawn: a process with its own memory, then exec), runs a thread
// (sharing its memory), and powers the machine off.
#include <fcntl.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/reboot.h>
#include <sys/wait.h>
#include <unistd.h>

static int kmsg = 2;
static int shared;

static void *thread(void *arg) {
  shared = 42;
  dprintf(kmsg, "spindle-hello: thread sees %s\n", (char *)arg);
  return (void *)7;
}

int main(int argc, char **argv) {
  kmsg = open("/dev/kmsg", O_WRONLY);
  if (argc > 1) {
    dprintf(kmsg, "spindle-hello: child %d says %s\n", getpid(), argv[1]);
    return 7;
  }
  dprintf(kmsg, "spindle-hello: init %d starts\n", getpid());
  pid_t pid;
  char *args[] = {"/init", "hi", NULL};
  int failed = posix_spawn(&pid, "/init", NULL, NULL, args, NULL);
  int status = 0;
  if (failed) dprintf(kmsg, "spindle-hello: posix_spawn failed: %s\n", strerror(failed));
  else waitpid(pid, &status, 0);
  dprintf(kmsg, "spindle-hello: child exited %d\n", WEXITSTATUS(status));
  pthread_t t;
  void *result = 0;
  pthread_create(&t, NULL, thread, "the arg");
  pthread_join(t, &result);
  dprintf(kmsg, "spindle-hello: thread returned %ld, shared %d\n", (long)result, shared);
  dprintf(kmsg, "spindle-hello: done\n");
  reboot(RB_POWER_OFF);
  return 0;
}
