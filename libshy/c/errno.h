/* The C view of the native header. On a hosted target __errno_location is the
   system libc's thread-local accessor; on Shy it is libshy's own global. */
#ifndef __SHY_C_ERRNO_H
#define __SHY_C_ERRNO_H
#include "../libc/errno.shyh"
#endif
