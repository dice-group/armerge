/* Stand-ins for libunwind: nothing references them, but armerge must keep them global */
void *_Unwind_GetIP(void *ctx) { return ctx; }

void _Unwind_Resume(void *exc) { (void)exc; }
