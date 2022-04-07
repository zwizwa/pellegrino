#include "userspace.h"
void entry(void) {
    // serial_open_port(0); // port 0 is already opened
    for (;;) {
        uint8_t buf[1] = {'!'};
        struct Slice s = { .ptr = buf, .len = 1};
        serial_read_write(0, &s);
    }
}
