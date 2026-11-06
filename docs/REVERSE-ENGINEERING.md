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

## What we need from the GNA reference (being gathered)

- [ ] A. BAR0 register map: CONTROL (+ "go" bit), STATUS (busy/done/error),
      descriptor/page-directory base register, interrupt mask. Numeric offsets.
- [ ] B. Descriptor (xNN / operation) struct layout, field by field.
- [ ] C. Submission sequence: register write order, kick-off, completion.
- [ ] D. Generation differences; PCI id list (confirm 0x1911 absent).
- [ ] E. Sample workloads: what Intel's own `samples/` actually run
      (GMM scoring / speech acoustic scoring / affine+activation layers).

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
