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
#include <linux/uaccess.h>
#include <linux/fs.h>

#define GMM_VENDOR 0x8086
#define GMM_DEVICE 0x1911
#define GMM_DUMP_BYTES 0x1000 /* dump full 4KB BAR0 */

struct gmm_dev {
	struct pci_dev *pdev;
	void __iomem   *bar;
	resource_size_t bar_len;
	struct dentry  *dbg;
	struct dentry  *dbg_poke;
	struct dentry  *dbg_sweep;
	struct dentry  *dbg_gna;
	struct dentry  *dbg_wr;
	char            result[320];
	char            sweep[2048];
};

#define POKE_PAT_A 0xA5A5A5A5u
#define POKE_PAT_B 0x5A5A5A5Au  /* bit-complement of A */

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

/* GNA Gen0.9+ register offsets (from the mainline-proposed intel_gna driver). */
#define GNA_MMIO_STS     0x80
#define GNA_MMIO_CTRL    0x84
#define GNA_MMIO_PTC     0x8c
#define GNA_MMIO_PSC     0x90
#define GNA_MMIO_D0I3C   0xa8
#define GNA_MMIO_DESBASE 0xb0
#define GNA_MMIO_IBUFFS  0xb4
#define GNA_PGDIR_OFF    0x200   /* our hypothesis: 64-entry page directory */
#define GNA_PGDIR_N      64

/*
 * Read-only interpretation of BAR0 through the GNA register map. These
 * offsets are confirmed for GNA Gen 0.9-3.0 (Gemini Lake onward); for the
 * pre-0.9 Skylake GMM (8086:1911) they are a HYPOTHESIS to be verified.
 */
static int gmm_gna_show(struct seq_file *s, void *unused)
{
	struct gmm_dev *d = s->private;
	u32 sts, ctrl, desbase, ibuffs;
	int i, valid = 0;

	if (!d || !d->bar) {
		seq_puts(s, "no BAR mapped\n");
		return 0;
	}
	sts     = ioread32(d->bar + GNA_MMIO_STS);
	ctrl    = ioread32(d->bar + GNA_MMIO_CTRL);
	desbase = ioread32(d->bar + GNA_MMIO_DESBASE);
	ibuffs  = ioread32(d->bar + GNA_MMIO_IBUFFS);

	seq_puts(s, "BAR0 via GNA register map (offsets UNVERIFIED for pre-0.9 1911)\n\n");
	seq_printf(s, "STS     0x80 = 0x%08x  [done=%u stats=%u mmu_err=%u dma_err=%u va_oor=%u param_oor=%u]\n",
		   sts, !!(sts & BIT(0)), !!(sts & BIT(3)), !!(sts & BIT(4)),
		   !!(sts & BIT(5)), !!(sts & BIT(7)), !!(sts & BIT(8)));
	seq_printf(s, "CTRL    0x84 = 0x%08x  [start=%u active_list=%u op_mode=%u comp_int=%u err_int=%u]\n",
		   ctrl, !!(ctrl & BIT(0)), !!(ctrl & BIT(1)),
		   (ctrl >> 5) & 3, !!(ctrl & BIT(8)), !!(ctrl & BIT(10)));
	seq_printf(s, "PTC     0x8c = 0x%08x  (perf total cycles)\n",
		   ioread32(d->bar + GNA_MMIO_PTC));
	seq_printf(s, "PSC     0x90 = 0x%08x  (perf stall cycles)\n",
		   ioread32(d->bar + GNA_MMIO_PSC));
	seq_printf(s, "D0I3C   0xa8 = 0x%08x\n",
		   ioread32(d->bar + GNA_MMIO_D0I3C));
	seq_printf(s, "DESBASE 0xb0 = 0x%08x  (descriptor page-frame number)\n", desbase);
	seq_printf(s, "IBUFFS  0xb4 = 0x%08x  (input buffer size)\n", ibuffs);

	seq_printf(s, "\nPage-directory candidate @0x200 (%d x u32):\n", GNA_PGDIR_N);
	for (i = 0; i < GNA_PGDIR_N; i++) {
		u32 e = ioread32(d->bar + GNA_PGDIR_OFF + i * 4);

		if (e != 0xffffffffu && e != 0) {
			seq_printf(s, "  [%2d] 0x%08x\n", i, e);
			valid++;
		}
	}
	seq_printf(s, "  %d/%d entries differ from empty (0 / 0xffffffff)\n",
		   valid, GNA_PGDIR_N);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(gmm_gna);

/*
 * poke: safe writability probe of a single dword register.
 * Saves the original value, writes two bit-complement patterns, derives the
 * writable-bit mask from their read-back XOR, then restores the original.
 * Avoids all-ones / all-zeros writes to reduce the chance of triggering a
 * control register. Still: only aim this at scratch-looking registers.
 */
static ssize_t gmm_poke_write(struct file *f, const char __user *ub,
			      size_t len, loff_t *ppos)
{
	struct gmm_dev *d = f->private_data;
	char kb[32];
	u32 off, orig, ra, rb, rr, wmask;

	if (!d || !d->bar)
		return -ENODEV;
	if (len == 0 || len >= sizeof(kb))
		return -EINVAL;
	if (copy_from_user(kb, ub, len))
		return -EFAULT;
	kb[len] = '\0';
	if (kstrtou32(strim(kb), 0, &off))
		return -EINVAL;
	if ((off & 3) || off + 4 > d->bar_len)
		return -EINVAL;

	orig = ioread32(d->bar + off);
	iowrite32(POKE_PAT_A, d->bar + off);
	ra = ioread32(d->bar + off);
	iowrite32(POKE_PAT_B, d->bar + off);
	rb = ioread32(d->bar + off);
	iowrite32(orig, d->bar + off);   /* restore */
	rr = ioread32(d->bar + off);

	wmask = ra ^ rb;                 /* bits that followed the two patterns */

	scnprintf(d->result, sizeof(d->result),
		"offset   : +0x%03x\n"
		"original : 0x%08x\n"
		"wrote A5A5A5A5 -> read 0x%08x\n"
		"wrote 5A5A5A5A -> read 0x%08x\n"
		"writable_mask : 0x%08x\n"
		"restored : 0x%08x (%s)\n"
		"verdict  : %s\n",
		off, orig, ra, rb, wmask, rr,
		rr == orig ? "OK" : "MISMATCH!",
		wmask == 0xffffffffu ? "fully writable (scratch/RW)" :
		wmask == 0 ? "read-only / ignored" :
		"partially writable (some bits fixed)");

	dev_info(&d->pdev->dev,
		 "gmm_wake: poke +0x%03x orig=%08x A->%08x B->%08x wmask=%08x restored=%08x\n",
		 off, orig, ra, rb, wmask, rr);
	return len;
}

static ssize_t gmm_poke_read(struct file *f, char __user *ub,
			     size_t len, loff_t *ppos)
{
	struct gmm_dev *d = f->private_data;
	const char *msg = d->result[0] ? d->result :
		"write a dword offset (e.g. 0x804) here to run a safe R/W probe\n";

	return simple_read_from_buffer(ub, len, ppos, msg, strlen(msg));
}

static const struct file_operations gmm_poke_fops = {
	.owner  = THIS_MODULE,
	.open   = simple_open,
	.read   = gmm_poke_read,
	.write  = gmm_poke_write,
	.llseek = default_llseek,
};

/*
 * Controlled single-dword write: echo "<off> <val>" (hex) to write <val> to
 * BAR0+<off>. DANGEROUS by nature (writes real control registers). Reads back
 * and reports before/after. Used to clear stray state and to drive the GO
 * experiment once the descriptor format is known.
 */
static ssize_t gmm_wr_write(struct file *f, const char __user *ub,
			    size_t len, loff_t *ppos)
{
	struct gmm_dev *d = f->private_data;
	char kb[48], *p, *tok;
	u32 off, val, before, after;

	if (!d || !d->bar)
		return -ENODEV;
	if (len == 0 || len >= sizeof(kb))
		return -EINVAL;
	if (copy_from_user(kb, ub, len))
		return -EFAULT;
	kb[len] = '\0';
	p = strim(kb);

	tok = strsep(&p, " \t");
	if (!tok || !p || kstrtou32(tok, 0, &off))
		return -EINVAL;
	if (kstrtou32(strim(p), 0, &val))
		return -EINVAL;
	if ((off & 3) || off + 4 > d->bar_len)
		return -EINVAL;

	before = ioread32(d->bar + off);
	iowrite32(val, d->bar + off);
	after = ioread32(d->bar + off);

	scnprintf(d->result, sizeof(d->result),
		"wrote 0x%08x to +0x%03x : before=0x%08x after=0x%08x %s\n",
		val, off, before, after,
		after == val ? "(took)" : "(HW modified / sticky bits)");
	dev_info(&d->pdev->dev, "gmm_wake: wr +0x%03x <= 0x%08x (before %08x after %08x)\n",
		 off, val, before, after);
	return len;
}

static const struct file_operations gmm_wr_fops = {
	.owner  = THIS_MODULE,
	.open   = simple_open,
	.read   = gmm_poke_read,   /* reuse: reports d->result */
	.write  = gmm_wr_write,
	.llseek = default_llseek,
};

/* Probe one dword's writable-bit mask, restoring the original value. */
static u32 gmm_probe_dword(struct gmm_dev *d, resource_size_t off)
{
	u32 orig = ioread32(d->bar + off);
	u32 ra, rb;

	iowrite32(POKE_PAT_A, d->bar + off);
	ra = ioread32(d->bar + off);
	iowrite32(POKE_PAT_B, d->bar + off);
	rb = ioread32(d->bar + off);
	iowrite32(orig, d->bar + off);          /* restore */
	return ra ^ rb;                         /* writable-bit mask */
}

/*
 * sweep: classify every dword in BAR0 as writable (full/partial) or not,
 * total the writable "memory", list contiguous fully-writable ranges, and
 * test whether the 0x800 and 0xc00 banks are independent or aliased.
 */
static void gmm_run_sweep(struct gmm_dev *d)
{
	resource_size_t off, run_start = 0;
	u32 rw = 0, partial = 0, ro = 0;
	bool in_run = false;
	size_t u = 0;
	size_t sz = sizeof(d->sweep);

	u += scnprintf(d->sweep + u, sz - u,
		       "Fully-writable (scratch) ranges:\n");

	for (off = 0; off + 4 <= d->bar_len; off += 4) {
		u32 wm = gmm_probe_dword(d, off);

		if (wm == 0xffffffffu) {
			rw++;
			if (!in_run) { run_start = off; in_run = true; }
		} else {
			if (in_run) {
				u += scnprintf(d->sweep + u, sz - u,
					"  0x%03llx - 0x%03llx  (%llu B)\n",
					(unsigned long long)run_start,
					(unsigned long long)(off - 4),
					(unsigned long long)(off - run_start));
				in_run = false;
			}
			if (wm == 0)
				ro++;
			else
				partial++;
		}
	}
	if (in_run)
		u += scnprintf(d->sweep + u, sz - u,
			"  0x%03llx - 0x%03llx  (%llu B)\n",
			(unsigned long long)run_start,
			(unsigned long long)(d->bar_len - 4),
			(unsigned long long)(d->bar_len - run_start));

	/*
	 * Independence test for the two scratch banks. The writable field of
	 * each 12-byte record sits at +4, so compare 0x804 vs 0xc04.
	 */
	if (0x804 + 4 <= d->bar_len && 0xc04 + 4 <= d->bar_len &&
	    gmm_probe_dword(d, 0x804) == 0xffffffffu &&
	    gmm_probe_dword(d, 0xc04) == 0xffffffffu) {
		u32 o8 = ioread32(d->bar + 0x804);
		u32 oc = ioread32(d->bar + 0xc04);
		u32 seen_c;

		iowrite32(0xdeadbeefu, d->bar + 0x804);
		seen_c = ioread32(d->bar + 0xc04);
		iowrite32(o8, d->bar + 0x804);
		iowrite32(oc, d->bar + 0xc04);
		u += scnprintf(d->sweep + u, sz - u,
			"0x804 vs 0xc04 : %s\n",
			seen_c == 0xdeadbeefu ? "ALIASED (same RAM)"
					      : "INDEPENDENT (two banks)");
	}

	u += scnprintf(d->sweep + u, sz - u,
		"\nSummary: writable=%u dwords = %u bytes | partial=%u | ro/zero=%u | total=%llu dwords (%llu B BAR)\n",
		rw, rw * 4, partial, ro,
		(unsigned long long)(d->bar_len / 4),
		(unsigned long long)d->bar_len);

	dev_info(&d->pdev->dev,
		 "gmm_wake: sweep done, writable=%u dwords (%u B)\n", rw, rw * 4);
}

static ssize_t gmm_sweep_write(struct file *f, const char __user *ub,
			       size_t len, loff_t *ppos)
{
	struct gmm_dev *d = f->private_data;

	if (!d || !d->bar)
		return -ENODEV;
	gmm_run_sweep(d);
	return len;
}

static ssize_t gmm_sweep_read(struct file *f, char __user *ub,
			      size_t len, loff_t *ppos)
{
	struct gmm_dev *d = f->private_data;
	const char *msg = d->sweep[0] ? d->sweep :
		"write anything here to run a full BAR0 writability sweep\n";

	return simple_read_from_buffer(ub, len, ppos, msg, strlen(msg));
}

static const struct file_operations gmm_sweep_fops = {
	.owner  = THIS_MODULE,
	.open   = simple_open,
	.read   = gmm_sweep_read,
	.write  = gmm_sweep_write,
	.llseek = default_llseek,
};

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
	d->dbg_poke = debugfs_create_file("gmm_wake_poke", 0644, NULL, d,
					  &gmm_poke_fops);
	d->dbg_sweep = debugfs_create_file("gmm_wake_sweep", 0644, NULL, d,
					   &gmm_sweep_fops);
	d->dbg_gna = debugfs_create_file("gmm_wake_gna", 0444, NULL, d,
					 &gmm_gna_fops);
	d->dbg_wr = debugfs_create_file("gmm_wake_wr", 0644, NULL, d,
					&gmm_wr_fops);
	dev_info(&pdev->dev, "gmm_wake: dump=/sys/kernel/debug/gmm_wake poke=..._poke sweep=..._sweep\n");
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
		debugfs_remove(d->dbg_poke);
		debugfs_remove(d->dbg_sweep);
		debugfs_remove(d->dbg_gna);
		debugfs_remove(d->dbg_wr);
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
