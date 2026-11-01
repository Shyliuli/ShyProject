/* The Shy target uses its own descriptor; hosted targets use the compiler's. */
#ifndef __SHY_STDARG_WRAPPER_H
#define __SHY_STDARG_WRAPPER_H
#if defined(__shy__)
#include <core/stdarg.shyh>
#else
#include <stdarg-hosted.h>
#endif
#endif
