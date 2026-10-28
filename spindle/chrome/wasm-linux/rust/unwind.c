// The libunwind Rust's std links on Spindle's wasm Linux. Programs there are built with
// panic=abort, and wasm keeps its call stack to itself, so there is nothing to unwind and
// no frames to walk: a backtrace ends at once, and raising an exception fails.
#include <stdint.h>

enum { URC_FATAL_PHASE1_ERROR = 3, URC_END_OF_STACK = 5 };

int _Unwind_Backtrace(void *trace, void *argument) { return URC_END_OF_STACK; }
uintptr_t _Unwind_GetIP(void *context) { return 0; }
uintptr_t _Unwind_GetIPInfo(void *context, int *before) {
    *before = 0;
    return 0;
}
uintptr_t _Unwind_GetCFA(void *context) { return 0; }
void *_Unwind_FindEnclosingFunction(void *pc) { return 0; }
int _Unwind_RaiseException(void *exception) { return URC_FATAL_PHASE1_ERROR; }
void _Unwind_Resume(void *exception) { __builtin_trap(); }
void _Unwind_DeleteException(void *exception) {}
uintptr_t _Unwind_GetLanguageSpecificData(void *context) { return 0; }
uintptr_t _Unwind_GetRegionStart(void *context) { return 0; }
uintptr_t _Unwind_GetTextRelBase(void *context) { return 0; }
uintptr_t _Unwind_GetDataRelBase(void *context) { return 0; }
void _Unwind_SetGR(void *context, int index, uintptr_t value) {}
void _Unwind_SetIP(void *context, uintptr_t value) {}
