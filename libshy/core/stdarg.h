/* The Shy target uses its own descriptor; hosted targets use the compiler's.
   This router stays hand-written: the Shy va_arg expansion has no C spelling. */
#ifndef __SHY_STDARG_WRAPPER_H
#define __SHY_STDARG_WRAPPER_H
#if defined(__shy__)
#include "stdarg.shyh"
#else
#include <stdarg-hosted.h>
#endif
#endif
