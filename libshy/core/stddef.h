/* size_t and ptrdiff_t follow __SIZEOF_POINTER__, which a generated header
   would freeze at one width. The native header is plain C, so include it. */
#ifndef __SHY_STDDEF_WRAPPER_H
#define __SHY_STDDEF_WRAPPER_H
#include "stddef.shyh"
#endif
