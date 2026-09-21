#include "AtomicFlag.h"

#include <stdatomic.h>
#include <stdlib.h>

struct StreameAtomicFlag {
    atomic_bool value;
};

StreameAtomicFlag *streame_flag_create(bool initial) {
    StreameAtomicFlag *flag = malloc(sizeof *flag);
    if (flag) {
        atomic_init(&flag->value, initial);
    }
    return flag;
}

void streame_flag_destroy(StreameAtomicFlag *flag) {
    free(flag);
}

bool streame_flag_load(const StreameAtomicFlag *flag) {
    return atomic_load_explicit(&flag->value, memory_order_acquire);
}

void streame_flag_store(StreameAtomicFlag *flag, bool value) {
    atomic_store_explicit(&flag->value, value, memory_order_release);
}
