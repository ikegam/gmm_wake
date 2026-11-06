// gmm_test - drive the Skylake GMM accelerator from userspace via /dev/gmm0.
// Build: cc -o gmm_test gmm_test.c   Run: ./gmm_test
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include "gmm_uapi.h"

static uint32_t cpu_ref(const struct gmm_job *j)
{
	uint32_t s = j->gconst;
	for (uint32_t i = 0; i < j->vlen; i++) {
		int16_t  d  = (int16_t)j->feature[i] - (int16_t)j->mean[i];
		uint16_t sq = (uint16_t)(d * d);
		s += (uint32_t)sq * j->var[i];
	}
	return s;
}

int main(void)
{
	int fd = open("/dev/gmm0", O_RDWR);
	if (fd < 0) { perror("open /dev/gmm0"); return 1; }

	int fail = 0;
	for (int t = 0; t < 3; t++) {
		struct gmm_job j;
		memset(&j, 0, sizeof j);
		j.vlen = 24;
		j.gconst = 1000 + t * 500;
		for (uint32_t i = 0; i < j.vlen; i++) {
			j.feature[i] = (uint8_t)(7 * i + 3 + t * 13);
			j.mean[i]    = (uint8_t)(3 * i + 11);
			j.var[i]     = (uint8_t)(1 + (i & 3));
		}
		if (ioctl(fd, GMM_COMPUTE, &j)) { perror("ioctl GMM_COMPUTE"); close(fd); return 1; }
		uint32_t ref = cpu_ref(&j);
		int ok = (j.score == ref);
		fail |= !ok;
		printf("run %d: HW=%u  CPU=%u  sts=0x%x  -> %s\n",
		       t, j.score, ref, j.hw_status, ok ? "PASS" : "FAIL");
	}
	close(fd);
	printf("%s\n", fail ? "SOME FAILED" : "ALL PASS - userspace drove the GMM chip");
	return fail;
}
