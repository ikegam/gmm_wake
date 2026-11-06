// gmm_wake - Wake and map the Intel Skylake "Gaussian Mixture Model" block.
// PCI 8086:1911 (00:08.0). Enables the device, maps BAR0 (4KB MMIO), and
// exposes a read-only register dump via debugfs. READ-ONLY: never writes to
// the undocumented MMIO, to avoid hanging the bus.
//
// Fully reversible: `rmmod gmm_wake` disables the device again.

#include <linux/module.h>
#include <linux/pci.h>
#include <linux/io.h>
#include <linux/seq_file.h>
#include <linux/debugfs.h>
#include <linux/slab.h>

#define GMM_VENDOR 0x8086
#define GMM_DEVICE 0x1911
#define GMM_DUMP_BYTES 0x1000 /* dump full 4KB BAR0 */

struct gmm_dev {
	struct pci_dev *pdev;
	void __iomem   *bar;
	resource_size_t bar_len;
	struct dentry  *dbg;
};

static int gmm_regs_show(struct seq_file *s, void *unused)
{
	struct gmm_dev *d = s->private;
	resource_size_t i, lim;
	int nonzero = 0;
	bool in_zero_run = false;

	if (!d || !d->bar) {
		seq_puts(s, "no BAR mapped\n");
		return 0;
	}
	seq_printf(s, "Intel GMM 8086:1911  BAR0 len=%llu bytes (non-zero only)\n",
		   (unsigned long long)d->bar_len);
	lim = min_t(resource_size_t, d->bar_len, (resource_size_t)GMM_DUMP_BYTES);
	for (i = 0; i + 4 <= lim; i += 4) {
		u32 v = ioread32(d->bar + i);
		if (v == 0) {
			if (!in_zero_run) {
				seq_puts(s, "  ....  (zeros)\n");
				in_zero_run = true;
			}
			continue;
		}
		in_zero_run = false;
		nonzero++;
		seq_printf(s, "  +0x%03llx : 0x%08x\n",
			   (unsigned long long)i, v);
	}
	seq_printf(s, "-- %d non-zero dwords in first 0x%x bytes --\n",
		   nonzero, (unsigned)lim);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(gmm_regs);

static int gmm_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
	struct gmm_dev *d;
	u32 r0;
	int rc;

	dev_info(&pdev->dev, "gmm_wake: found %04x:%04x, waking it up\n",
		 pdev->vendor, pdev->device);

	d = devm_kzalloc(&pdev->dev, sizeof(*d), GFP_KERNEL);
	if (!d)
		return -ENOMEM;
	d->pdev = pdev;

	rc = pci_enable_device(pdev);        /* enable 0 -> 1 */
	if (rc) {
		dev_err(&pdev->dev, "gmm_wake: pci_enable_device failed: %d\n", rc);
		return rc;
	}

	rc = pci_request_region(pdev, 0, "gmm_wake");
	if (rc) {
		dev_err(&pdev->dev, "gmm_wake: request BAR0 failed: %d\n", rc);
		goto err_disable;
	}

	d->bar_len = pci_resource_len(pdev, 0);
	d->bar = pci_iomap(pdev, 0, 0);
	if (!d->bar) {
		dev_err(&pdev->dev, "gmm_wake: iomap BAR0 failed\n");
		rc = -ENOMEM;
		goto err_release;
	}

	pci_set_drvdata(pdev, d);

	r0 = ioread32(d->bar + 0);           /* first proof-of-life read */
	dev_info(&pdev->dev,
		 "gmm_wake: ALIVE. BAR0 mapped len=%llu, reg[0x00]=0x%08x\n",
		 (unsigned long long)d->bar_len, r0);

	d->dbg = debugfs_create_file("gmm_wake", 0444, NULL, d, &gmm_regs_fops);
	dev_info(&pdev->dev, "gmm_wake: register dump at /sys/kernel/debug/gmm_wake\n");
	return 0;

err_release:
	pci_release_region(pdev, 0);
err_disable:
	pci_disable_device(pdev);
	return rc;
}

static void gmm_remove(struct pci_dev *pdev)
{
	struct gmm_dev *d = pci_get_drvdata(pdev);

	if (d) {
		debugfs_remove(d->dbg);
		if (d->bar)
			pci_iounmap(pdev, d->bar);
		pci_release_region(pdev, 0);
	}
	pci_disable_device(pdev);
	dev_info(&pdev->dev, "gmm_wake: released and disabled\n");
}

static const struct pci_device_id gmm_ids[] = {
	{ PCI_DEVICE(GMM_VENDOR, GMM_DEVICE) },
	{ 0 }
};
MODULE_DEVICE_TABLE(pci, gmm_ids);

static struct pci_driver gmm_driver = {
	.name     = "gmm_wake",
	.id_table = gmm_ids,
	.probe    = gmm_probe,
	.remove   = gmm_remove,
};
module_pci_driver(gmm_driver);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("gmm_wake experiment");
MODULE_DESCRIPTION("Wake & map Intel Skylake GMM (8086:1911)");
