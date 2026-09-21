// Drapeau atomique C11 pour les callbacks audio temps réel (Swift n'expose pas d'atomique
// sans dépendance avant iOS 18) : lecture « acquire », écriture « release », sans verrou.
#ifndef STREAME_ATOMIC_FLAG_H
#define STREAME_ATOMIC_FLAG_H

#include <stdbool.h>

typedef struct StreameAtomicFlag StreameAtomicFlag;

StreameAtomicFlag *streame_flag_create(bool initial);
void streame_flag_destroy(StreameAtomicFlag *flag);
bool streame_flag_load(const StreameAtomicFlag *flag);
void streame_flag_store(StreameAtomicFlag *flag, bool value);

#endif
