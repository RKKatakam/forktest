#include <notify.h>
#include <stdint.h>

/* After fork() a child cannot reach notifyd, so libc's time zone change check fails on every localtime() call and
   reloads the zone file each time. A failed check is reported as "no change", which keeps libc's cached zone. */
static uint32_t forktest_notify_check(int token, int *check) {
    uint32_t status = notify_check(token, check);
    if (status != NOTIFY_STATUS_OK) {
        if (check) *check = 0;
        return NOTIFY_STATUS_OK;
    }
    return status;
}

__attribute__((used)) static const struct {
    const void *replacement;
    const void *replacee;
} interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)forktest_notify_check, (const void *)notify_check},
};
