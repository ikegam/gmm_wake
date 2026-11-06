# Reverse-engineering the Skylake GMM (8086:1911)

Goal: make the chip actually *compute*, by using the later Intel GNA
(Gen 2.0/3.0) driver + library as a reference design and extrapolating back
to this Gen-0.9 / "GMM" part. No kernel driver anywhere binds 0x1911, so the
only path is to learn the GNA programming model and adapt it.

## Observed BAR0 map (4 KB) and working hypotheses

From our own read/write sweep (see README). Hypotheses are **unconfirmed**
until checked against the GNA reference source.

| Region        | Observed                                   | Hypothesis                                                        |
|---------------|--------------------------------------------|------------------------------------------------------------------|
| `0x000–0x08c` | small ints: `0x01`, `0x18`(24), `0x0f`(15), `0x04`, `0x08` | **Capability/param block** — engine counts, buffer sizes advertised by HW |
| `0x200–0x2fc` | 256 B, writable, default **all-`0xffffffff`** | **Page directory** — 64 × 4-byte physical-page entries; all-ones = empty/invalid. The MMU the HW uses to reach descriptors + data in RAM |
| `0x804..0x834`/`0xc04..0xc34` | 12-byte records, writable field at +4; `0xc00` ALIASES `0x800` | **Control/queue descriptor** for one request context (two address windows, one RAM) |
| scattered `0x054,0x0a0,0x404` | writable dwords                    | candidate CONTROL / STATUS / doorbell registers                  |

## GNA reference spec (obtained)

From the mainline-proposed `drivers/misc/intel/gna/` (lore.kernel.org v5,
2022-10-20) and the archived `github.com/intel/gna` userspace library.

### A. BAR0 register map (GNA Gen 0.9-3.0; UNVERIFIED for pre-0.9 1911)

| Off  | Name    | Meaning                                                        |
|------|---------|----------------------------------------------------------------|
| 0x80 | STS     | bit0=done, b3=stats_valid, b4=MMU_err, b5=DMA_err, b7=VA_OOR, b8=PARAM_OOR, b17=saturate |
| 0x84 | CTRL    | **bit0=START_ACCEL (GO)**, b1=active_list_en, b2=abort, **b5-6=OP_MODE (0=GMM,1=XNN)**, b8=comp_int_en, b10=err_int_en, b12-15=perf |
| 0x8c | PTC     | perf total cycles                                              |
| 0x90 | PSC     | perf stall cycles                                              |
| 0xa8 | D0I3C   | D0i3 power control                                             |
| 0xb0 | DESBASE | **PFN of the 784-byte hw descriptor**                          |
| 0xb4 | IBUFFS  | input buffer size                                              |

Single MSI vector; no separate IRQ-mask register (IRQs enabled by CTRL bits).

### B. Descriptor (784 bytes, pointed to by DESBASE PFN)

- `+256 xnn_config`: `u32 labase` (layer-array GNA-virtual addr), `u16 lacount`.
- `+512 gna_mmu`: `u32 vamaxaddr`, then **`u32 pagedir_n[64]`** — a 64-entry
  page directory; each entry → a page table of up to 1024 u32 page addresses.
  Two-level MMU, 64 page tables, max 256 MB mapped.
- Per-layer op descriptors live in GNA-mapped memory at `labase`; their binary
  format is opaque (emitted by the userspace library, not in the kernel).

### C. Submission sequence

1. Pin/map buffers; apply pointer patches; fill page tables (`gna_mmu_add`).
2. Build descriptor: set `xnn_config.labase/lacount` (XNN) or copy GMM config.
3. `DESBASE = hwdesc_dma >> PAGE_SHIFT`.
4. `CTRL`: set START_ACCEL(b0) | COMP_INT_EN(b8) | ERR_INT_EN(b10) | OP_MODE |
   perf/active-list bits, then write CTRL. **Writing bit0 is the GO.**
5. Wait for completion interrupt (ISR reads STS, clears busy).
6. Read STS: error bits → -EIO; bit0 set → done; else -ENODATA.
7. Abort: set CTRL b2, poll STS bit0 clear.

### D. Generations / PCI IDs

Kernel binds: GLK 0x3190, CNL 0x5A11, EHL 0x4511, ICL 0x8A11 (Gen1);
JSL 0x4E11, TGL 0x9A11, RKL 0x4C11, ADL 0x464F, RPL 0xA74F (Gen2).
**0x1911 is NOT bound by any kernel driver.** Userspace lib enum:
`Gna2DeviceVersionGMM = 0x01` (our chip) predates `0_9=0x09`. GEN1 caps:
max_layer_count=1024, num_pagetables=64, max_hw_mem=256MB.

### E. Use case / hello-world

GNA = low-power neural coprocessor; OpenVINO GNA plugin used it for **speech
acoustic-model scoring** and **noise reduction**. Only shipped sample
(`samples/src/sample01/sample01.cpp`) = **one FullyConnectedAffine layer**:
`out = W·in + bias`, W 8×16 int16, in 16×4 int16, bias 8 int32, out 8×4 int32.
Reference first output row: `-177 -85 29 28`. That matmul is our target job.

## Empirical finding: 0x84 is a latching control register

Writing `0x0` to `0x84` does NOT clear it (before=0x9, after=0x9). The value
`0x9 = bit0|bit3` is exactly what our earlier sweep latched: `0xA5A5A5A5` set
bit0, `0x5A5A5A5A` set bit3. So **0x84 is a write-1-to-set / latching control
register, not scratch RAM** — strong evidence the GNA map (STS 0x80 / CTRL
0x84) applies to this pre-0.9 part. bit0 (START candidate) is currently
latched high, yet STS(0x80)=0 with no error — consistent with "START asserted
but no valid descriptor/page-dir, so nothing executes."

Consequence: the chip is now in a dirtied control state from the sweep. Do a
**clean reboot before the real job attempt** so CTRL/STS start from power-on
defaults. To clear without reboot, try the GNA ABORT path (CTRL bit2) or W1C.

## KEY MATCH: 0x200 == 64-entry page directory

`GNA_PGDIRN_LEN = 64`. Our writable block `0x200-0x2fc` = 256 B = **64 × u32**,
default all-`0xffffffff` (empty). This strongly supports: **the pre-0.9 GMM
keeps its page directory in BAR0 MMIO** (vs GNA 0.9+ which keeps it in the RAM
descriptor). The `0x800`/`0xc00` 12-byte records may be the in-BAR
xnn/layer descriptor. Control/status likely live in `0x00-0x08c` (our dense
low block), NOT at GNA's 0x80/0x84 — those read 0 on our chip.

## Plan once the reference lands

1. Map observed offsets → GNA register names. Confirm `0x200` = page directory.
2. Pick the simplest sample workload (likely a single GMM-scoring or
   affine-layer op) as the "hello world".
3. In the driver: allocate DMA buffers for the descriptor + input + output,
   fill the page directory (`0x200`) with their physical pages, write the
   descriptor, program the base register, set the "go" bit, poll STATUS.
4. Read back output; compare against a CPU reference computation of the same op.

## Reality check

Even a success only runs tiny fixed-function speech/affine ops — not an LLM.
The win is making a driverless, undocumented 2016 chip perform a real
computation under our own code. Expect dead-ends: Gen-0.9 register offsets
and descriptor format may differ from Gen-2.0+, and some control bits may be
unknowable without a logic-level trace.

## SOLVED: first successful GMM computation

The chip computes. Journey of the GO attempts (STS 0x80 after START):

| Setup | STS err | meaning |
|-------|---------|---------|
| config in RAM descriptor, RAM MMU | `0x180` | VA_OOR + PARAM_OOR |
| **config in BAR0 0x00** (correct params) | `0x080` | PARAM_OOR cleared; VA_OOR remains |
| + various MMU / address schemes | `0x080` | VA_OOR stubborn |
| **+ `0x404` = 0x7fffffff** | `0x000` | **done=1, score written** |

**Key discoveries for the pre-0.9 Skylake GMM (`8086:1911`):**

1. **GMM_CONFIG lives in BAR0 MMIO at `0x00`** (128 bytes), not in a RAM
   descriptor. Confirmed: power-on width fields match GMM constants
   (fvwidth@0x08=1, mvwidth@0x20=1, gcwidth@0x4c=4, score widths@0x58/0x74=4),
   and our written config reads back and clears PARAM_OOR. STS/CTRL sit right
   after it at 0x80/0x84.
2. **`BAR0 0x404` is the address-range limit (vamaxaddr).** Its small power-on
   default makes every descriptor address fail with VA_OOR. Writing a large
   value lets scoring run. Found by probing the handful of writable scalar
   registers (`0x0a0`, `0x404`).
3. **Register map matches GNA:** STS `0x80` (bit0=done, bit7=VA_OOR,
   bit8=PARAM_OOR), CTRL `0x84` (bit0=START, bits5-6=OP_MODE, bit2=abort),
   DESBASE `0xB0`. CTRL is write-1-to-set/latching.
4. **MMU:** 2-level page directory (BAR0 0x200 and/or RAM descriptor via
   DESBASE), PTE = phys>>PAGE_SHIFT, data at virtual 0x1000 (virtual 0 reserved).

**Verification:** feature==mean (distance 0) → score == gconst exactly;
gconst 1000→score 1000, gconst 4660→score 4660. The chip reads our mapped input
and writes the mapped output. ~10 µs compute time vs instant error rejection.

**Open:** the feature-dependent distance term is still ~0 (score stays at
gconst when the feature is perturbed) — the gconst-dominated minimal config, or
a variance/stride encoding detail, still to be worked out.
