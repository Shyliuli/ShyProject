/* `va_list` for the active target. */
#ifndef __SHYCC_STDARG_WRAPPER_H
#define __SHYCC_STDARG_WRAPPER_H
#if defined(__shy__)
#include <stdarg.shyh>
#else
#include <stdarg-hosted.h>
#endif
#endif
