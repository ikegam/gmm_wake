# gmm_wake

Wake up and map the Intel **Gaussian Mixture Model (GMM)** block —
PCI `8086:1911`, the Skylake-era predecessor of Intel GNA — on a machine
where it sits disabled with **no driver anywhere on Earth** that binds to it.

This is a reverse-engineering / "old hardware shining again" experiment on a
Lenovo ThinkPad (Intel Core i7-6600U, Skylake). The chip is a fixed-function
low-power accelerator with no public documentation and no surviving software
stack (Intel's GNA driver/library floor is Cannon Lake `0x5A11`; the OpenVINO
GNA plugin was removed). So we write our own minimal PCI driver to claim it,
power it on, map its MMIO, and read its registers.

**It does not run an LLM. It cannot — the chip is not a general compute
engine.** The goal is to take ownership of a dead, disabled chip and see it
respond.

## What it does

The module (`gmm_wake.c`):

1. Registers a PCI driver matching `8086:1911`.
2. `pci_enable_device()` — flips the device `enable` from `0` to `1`.
3. `pci_request_region()` + `pci_iomap()` — maps BAR0 (4 KB MMIO).
4. Reads registers (read-only; never writes undocumented MMIO) and exposes a
   dump at `/sys/kernel/debug/gmm_wake`.

Fully reversible: `rmmod gmm_wake` releases and disables the device again.

## Build & run

```sh
make
sudo insmod ./gmm_wake.ko
cat /sys/bus/pci/devices/0000:00:08.0/enable        # -> 1
readlink /sys/bus/pci/devices/0000:00:08.0/driver   # -> .../gmm_wake
sudo cat /sys/kernel/debug/gmm_wake                 # register dump
sudo rmmod gmm_wake                                  # undo everything
```

Requires kernel headers (`/lib/modules/$(uname -r)/build`). Loading taints the
kernel (out-of-tree, unsigned) — harmless for an experiment.

## Findings so far (BAR0 register map, 4 KB)

Read-only survey of BAR0 on a live, D0-powered chip. Zero regions omitted.

| Region        | Contents                                          | Hypothesis (unconfirmed)                      |
|---------------|---------------------------------------------------|-----------------------------------------------|
| `0x000–0x08c` | `0x01` / `0x18`(24) scattered, `0x088=0x0f`(15)   | Config / capability block (resource counts)   |
| `0x200–0x2fc` | 64 dwords, **all `0xffffffff`**                    | Mask bank, default = all-masked               |
| `0x800–0x834` | `0x76543210` × 5, stride 12 bytes                 | Scratch registers (descending-nibble default) |
| `0xc00–0xc34` | identical mirror of `0x800` block                 | Second engine/queue context                   |

`0x76543210` is a classic scratch-register default pattern → candidate
writable registers. The `0x800` / `0xc00` mirror suggests two identical engine
contexts.

### Observed register dump

```
+0x008 : 0x00000001
+0x010 : 0x00000001
+0x014 : 0x00000018
+0x020 : 0x00000001
+0x028 : 0x00000018
+0x038 : 0x00000001
+0x040 : 0x00000018
+0x04c : 0x00000004
+0x050 : 0x00000008
+0x058 : 0x00000004
+0x05c : 0x00000001
+0x064 : 0x00000001
+0x070 : 0x00000001
+0x074 : 0x00000004
+0x088 : 0x0000000f
+0x200..0x2fc : 0xffffffff (all)
+0x804,0x810,0x81c,0x828,0x834 : 0x76543210
+0xc04,0xc10,0xc1c,0xc28,0xc34 : 0x76543210
```

## Status

- [x] Bind a custom driver to `8086:1911`
- [x] Power the chip on (`enable 0→1`, `power_state=D0`)
- [x] Map BAR0 and read live registers
- [x] Map the 4 KB register layout
- [ ] Confirm writable scratch registers (write/read-back test) — next
- [ ] Reverse the scoring descriptor / command protocol — far future

## Safety

Read path is safe. Any future write path targets only scratch-looking
registers, saves and restores the original value, and is opt-in. Writing
undocumented MMIO can hang the bus — treat with care.
