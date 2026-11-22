/* mmap and munmap for ripgrep on Spindle's wasm Linux, which has no mmap (musl declares
 * them; the kernel has nothing to map with). ripgrep only maps a file when that may be
 * faster, and reads it when mapping fails, as it does here: every call fails with
 * ENOSYS, as the system call would. build-image.sh links this into ripgrep alone. */
#include <errno.h>
#include <sys/mman.h>

void *mmap(void *address, size_t length, int protection, int flags, int fd, off_t offset)
{
    (void)address; (void)length; (void)protection; (void)flags; (void)fd; (void)offset;
    errno = ENOSYS;
    return MAP_FAILED;
}

int munmap(void *address, size_t length)
{
    (void)address; (void)length;
    errno = ENOSYS;
    return -1;
}
