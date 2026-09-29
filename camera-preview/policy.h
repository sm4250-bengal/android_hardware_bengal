/* SPDX-License-Identifier: Apache-2.0 */
#ifndef BENGAL_CAMERA_POLICY_H
#define BENGAL_CAMERA_POLICY_H
#include <stdbool.h>
int camera_policy_start(unsigned int owner_uid, int reverse_line);
bool camera_policy_allowed(bool automatic);
bool camera_policy_begin(bool automatic);
void camera_policy_streaming(bool busy);
void camera_policy_stop(void);
#endif
