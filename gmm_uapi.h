/* SPDX-License-Identifier: GPL-2.0 */
/*
 * gmm_uapi.h - userspace interface for /dev/gmm0 (Intel Skylake GMM, 8086:1911).
 *
 * One synchronous ioctl: GMM_COMPUTE. Fill in a feature vector, a GMM model
 * (mean/variance per element + a Gaussian constant), and the vector length;
 * the hardware returns the GMM score
 *     score = gconst + Sum_j var[j] * (feature[j] - mean[j])^2   (uint32)
 * The calling thread sleeps (the chip wakes it by interrupt) until done.
 */
#ifndef _GMM_UAPI_H
#define _GMM_UAPI_H

#include <linux/types.h>
#include <linux/ioctl.h>

#define GMM_MAX_VLEN 96   /* hardware max; valid: 24,32,40,48,56,72,80,88,96 */

struct gmm_job {
	__u32 vlen;                   /* in: vector length (one of the valid set) */
	__u32 gconst;                 /* in: Gaussian constant (additive bias)    */
	__u8  feature[GMM_MAX_VLEN];  /* in: feature vector (uint8 per element)   */
	__u8  mean[GMM_MAX_VLEN];     /* in: model means                          */
	__u8  var[GMM_MAX_VLEN];      /* in: model inverse covariances            */
	__u32 score;                  /* out: hardware GMM score                  */
	__u32 hw_status;              /* out: raw STS register                    */
};

#define GMM_IOC_MAGIC 'G'
#define GMM_COMPUTE   _IOWR(GMM_IOC_MAGIC, 1, struct gmm_job)

#endif /* _GMM_UAPI_H */
