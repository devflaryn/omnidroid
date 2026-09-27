/* The one type of libcutils' <cutils/native_handle.h> the HAL headers use, as it is defined there
 * (system/core/libcutils/include/cutils/native_handle.h). The NDK does not ship libcutils. */
#pragma once
typedef struct native_handle {
    int version; /* sizeof(native_handle_t) */
    int numFds;
    int numInts;
    int data[0];
} native_handle_t;
typedef const native_handle_t* buffer_handle_t;
