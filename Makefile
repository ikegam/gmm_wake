obj-m := gmm_wake.o
KDIR  := /lib/modules/$(shell uname -r)/build
PWD   := $(shell pwd)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules
	$(CC) -O2 -Wall -o gmm_test gmm_test.c

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -f gmm_test
