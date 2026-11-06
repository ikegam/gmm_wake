# gmm_wake

**Making a driverless, forgotten 2016 Intel accelerator compute again — on Linux, from scratch.**

`gmm_wake` is a hand-written Linux kernel module that takes the Intel **Gaussian
Mixture Model (GMM)** accelerator — PCI `8086:1911`, the Skylake-era (pre-0.9)
ancestor of Intel GNA — from *disabled, no driver anywhere on Earth* to
**running a real GMM scoring computation whose result is verified, bit-exact,
against a CPU reference.**

Hardware: a Lenovo ThinkPad with an Intel Core i7-6600U (Skylake). The chip is a
fixed-function, ultra-low-power accelerator originally built for always-on
**speech recognition / wake-word detection** (Gaussian Mixture Model acoustic
scoring). Its software stack is long dead — Intel's mainline GNA driver floor is
Cannon Lake (`0x5A11`); `0x1911` is bound by **no kernel driver**, and the
OpenVINO GNA plugin was removed. So we wrote our own.

> **It does not run an LLM and cannot — it is not a general compute engine.**
> The win is making undocumented, abandoned silicon perform a real computation
> under code we wrote.

---

## Result — verified against a CPU reference

The module computes a CPU reference of the exact GMM kernel math from the same
input buffer and compares it to what the hardware wrote:

```
FIRED.
  STS 0x80 after=0x00000001  done=1 err=0x0 (~10 us)
  HW score  = 142760 (0x00022da8)
  CPU ref   = 142760 (= gconst + Sum var*(feat-mean)^2 over 24 elems)
  VERDICT   : PASS - hardware GMM result matches CPU reference
```

The score the hardware writes is **bit-exact** with
`gconst + Σ_j var[j]·(feature[j] − mean[j])²` (unsigned 32-bit, the real GNA GMM
formula), across arbitrary feature/mean/variance data. The accelerator reads all
24 feature elements, 24 means and 24 variances through the MMU we built, does the
squared-difference-times-variance accumulation, adds the per-mixture Gaussian
constant, and writes the result — in ~10 µs. This is a genuine, feature-dependent
computation, not a constant passthrough.

**The bug that hid it for a while:** the GMM hardware's minimum vector length is
**24** (valid lengths `{24,32,40,48,56,72,80,88,96}`); `8` is a *reserved* value.
With an invalid `vlength` the element loop runs zero times, so the chip emits
exactly `gconst` and never reads the operands — which looked like success until
we perturbed the inputs and nothing changed. Raising `vlength` to 24 turned on
the real arithmetic.

## How it works

1. **Bind & wake.** A PCI driver claims `8086:1911`, `pci_enable_device()`
   (enable 0→1, power state D0), maps BAR0 (4 KB MMIO).
2. **Program the operation in MMIO.** Unlike GNA 0.9+ (which keeps its
   descriptor in RAM), this pre-0.9 part holds its **128-byte GMM_CONFIG in BAR0
   at offset `0x00`** (its power-on width fields match the GMM element-size
   constants exactly). We write feature/mean/variance/gconst pointers (as
   GNA-virtual addresses), counts and sizes there.
3. **Set up the MMU.** A 2-level page directory (`BAR0 0x200` + a RAM descriptor
   via `DESBASE 0xB0`) maps our DMA data page into GNA-virtual space. PTE =
   `phys >> PAGE_SHIFT`; the data lives at virtual `0x1000` (virtual 0 is
   reserved).
4. **Open the address-range limit.** `BAR0 0x404` is this chip's **vamaxaddr**
   (address-range limit). Left at its small power-on default it rejects every
   address with `VA_OOR`; opening it up lets scoring run. *(Found by probing the
   few writable scalar registers.)*
5. **Fire.** Write `START` (CTRL `0x84` bit0, OP_MODE=GMM) and poll STS `0x80`
   for completion (bit0) / error bits. Read the score from the data page.

The register map (STS `0x80`, CTRL `0x84`, DESBASE `0xB0`) was extrapolated from
the later mainline Intel GNA driver and the Intel GNA userspace library, then
verified empirically against this Skylake stepping.

## Build & run

```sh
make                       # needs kernel headers at /lib/modules/$(uname -r)/build
sudo insmod ./gmm_wake.ko
echo build | sudo tee /sys/kernel/debug/gmm_wake_run   # set up DMA + descriptor
echo go    | sudo tee /sys/kernel/debug/gmm_wake_run   # fire the GMM op
sudo cat /sys/kernel/debug/gmm_wake_run                # read the score
sudo rmmod gmm_wake                                    # undo everything
```

debugfs interface (`/sys/kernel/debug/gmm_wake*`): `gmm_wake` (register dump),
`_run` (build/go a GMM job), `_gna` (GNA-map decode), `_poke` / `_wr` (register
probe/write), `_sweep` (writability map). `gmmctl` is a helper for the
build/go/poke cycle.

## Userland: `/dev/gmm0`

The module exposes a character device with one synchronous ioctl, `GMM_COMPUTE`
(see [`gmm_uapi.h`](gmm_uapi.h)). Fill in a feature vector and a GMM model
(mean/variance per element + a Gaussian constant); the call blocks — the CPU
**sleeps until the chip's interrupt wakes it** — and returns the score.

```c
int fd = open("/dev/gmm0", O_RDWR);
struct gmm_job j = { .vlen = 24, .gconst = 1000 };
/* fill j.feature[], j.mean[], j.var[] ... */
ioctl(fd, GMM_COMPUTE, &j);
/* j.score = gconst + Sum var*(feature-mean)^2, computed by the chip */
```

`gmm_test.c` is a reference user program (`make` builds it) that submits jobs and
checks each against a CPU reference:

```
run 0: HW=142760  CPU=142760  sts=0x1  -> PASS
run 1: HW=215800  CPU=215800  sts=0x1  -> PASS
run 2: HW=309120  CPU=309120  sts=0x1  -> PASS
ALL PASS - userspace drove the GMM chip
```

This is the chip's real usage model: a userspace process arms a job and idles
(the accelerator does the math at low power and signals completion by interrupt).
The planned market-watch app will be a Rust userland on top of this `/dev/gmm0`.

## Milestones

- [x] Bind a custom driver to `8086:1911` (no driver exists anywhere)
- [x] Wake the chip (enable 0→1, D0) and map BAR0
- [x] Read/write BAR0; map the 4 KB register layout
- [x] Confirm the GNA register map (STS/CTRL/START) applies to pre-0.9 silicon
- [x] Locate GMM_CONFIG in BAR0 `0x00` and the page directory in BAR0 `0x200`
- [x] Discover `0x404` = the address-range limit (vamaxaddr)
- [x] Run a GMM scoring op to completion (`done=1`) with a working data path
- [x] **Compute a real, feature-dependent GMM score, bit-exact vs a CPU
      reference** (the fix: `vlength` must be ≥ 24; 8 is reserved)
- [x] Interrupt-driven completion — the CPU sleeps until the chip signals
- [x] Userland `/dev/gmm0` (ioctl `GMM_COMPUTE`), verified from a user program
- [ ] Repurpose as a market-wave "wake-word" detector (Rust on `/dev/gmm0`)

## ⚠️ Warnings

- Reverse-engineered for *this* chip/stepping (i7-6600U Skylake GMM). Offsets may
  differ on other parts.
- Firing writes CTRL START on undocumented silicon. A bad descriptor can **hang
  the bus → power reboot required.** `build` quiesces (CTRL abort) first; a
  completed op settles in ~10 µs.
- Loading taints the kernel (out-of-tree, unsigned). Everything is reversible
  with `rmmod` / reboot.

## What's next

The original purpose of this chip was an always-on, ultra-low-power **"ear"** for
speech. The plan is to repurpose that same pattern — *watch a stream, score it,
wake someone when something stands out* — on **market price waves** instead of
audio: GMM-score sudden moves and send an alert. (Userspace in Rust.) It only
counts if it runs on this chip.

See [`docs/REVERSE-ENGINEERING.md`](docs/REVERSE-ENGINEERING.md) for the full
register map, descriptor format, MMU layout, and the debugging trail.

---
---

# gmm_wake（日本語）

**ドライバも存在しない、忘れられた2016年のIntelアクセラレータを、Linux上でゼロから再び計算させる。**

`gmm_wake` は自作のLinuxカーネルモジュールで、Intel **Gaussian Mixture Model
(GMM)** アクセラレータ——PCI `8086:1911`、Intel GNAのSkylake世代(pre-0.9)の祖先
——を、*無効・地球上にドライバ皆無* の状態から、**実際のGMMスコアリング演算を実行し、
正しい結果を書き戻す**ところまで動かします。

ハード:Intel Core i7-6600U(Skylake)搭載のLenovo ThinkPad。このチップは、常時オンの
**音声認識 / ウェイクワード検出**(混合ガウス分布による音響スコアリング)のために作られた
固定機能・超低消費電力アクセラレータ。対応ソフトはとうに消滅——Intelのmainline GNA
ドライバの下限はCannon Lake(`0x5A11`)で、`0x1911` は**どのカーネルドライバにも拾われず**、
OpenVINOのGNAプラグインも廃止済み。だから自分で書きました。

> **LLMは動かせないし、原理的に無理です——汎用演算器ではありません。**
> 価値は、資料もドライバも無い放棄されたシリコンに、自分で書いたコードで
> 本物の計算をさせたこと。

---

## 結果 — CPU参照とビット一致で検証

モジュールは同じ入力バッファからGMMカーネルの式をCPUでも計算し、ハードの書いた値と突き合わせます:

```
FIRED.
  STS 0x80 after=0x00000001  done=1 err=0x0 (~10us)
  HW score  = 142760 (0x00022da8)
  CPU ref   = 142760 (= gconst + Sum var*(feat-mean)^2 over 24 elems)
  VERDICT   : PASS - hardware GMM result matches CPU reference
```

ハードが書くスコアは、任意の feature/mean/variance に対して
`gconst + Σ_j var[j]·(feature[j] − mean[j])²`(符号なし32bit、本物のGNA GMM式)と
**ビット単位で一致**。アクセラレータは24要素の特徴・平均・分散をMMU経由で読み、
二乗差×分散を積算し、混合ごとのガウス定数を足して、約10µsで結果を書く。
**定数パススルーではない、本物の特徴依存演算。**

**しばらく隠れていたバグ**:GMMハードの最小ベクトル長は **24**(有効値
`{24,32,40,48,56,72,80,88,96}`)で、`8` は*予約値*。無効な `vlength` だと要素ループが
0回になり、チップはgconstちょうどを出力してオペランドを読まない——入力を揺らすまで
成功に見えてしまった。`vlength` を24に上げて本物の演算が点火した。

## 仕組み

1. **掌握と起動。** PCIドライバが `8086:1911` を掴み、`pci_enable_device()`
   (enable 0→1、電源D0)、BAR0(4KB MMIO)をマップ。
2. **演算をMMIOに書く。** GNA 0.9+(ディスクリプタをRAMに置く)と違い、このpre-0.9は
   **128バイトのGMM_CONFIGをBAR0の `0x00`** に持つ(電源オン時の幅フィールドがGMMの
   要素サイズ定数と完全一致)。特徴/平均/分散/gconstのポインタ(GNA仮想アドレス)、
   個数、サイズをそこに書く。
3. **MMU構築。** 2段ページディレクトリ(`BAR0 0x200` ＋ `DESBASE 0xB0` 経由のRAM
   ディスクリプタ)で、DMAデータページをGNA仮想空間にマップ。PTE = `物理 >> PAGE_SHIFT`、
   データは仮想 `0x1000`(仮想0は予約)。
4. **アドレス上限を開ける。** `BAR0 0x404` がこのチップの **vamaxaddr**(アドレス範囲
   上限)。電源オン時の小さな既定値のままだと全アドレスが `VA_OOR` で弾かれる。開放すると
   スコアリングが走る。*(書き込み可能な数少ないスカラレジスタを総当たりして発見。)*
5. **発火。** CTRL `0x84` のSTART(bit0、OP_MODE=GMM)を書き、STS `0x80` の完了(bit0)/
   エラービットをポーリング。データページからスコアを読む。

レジスタ配置(STS `0x80`、CTRL `0x84`、DESBASE `0xB0`)は、後世のmainline Intel GNA
ドライバとIntel GNAユーザー空間ライブラリから外挿し、このSkylakeステッピングで実測検証
しました。

## ビルドと実行

```sh
make                       # /lib/modules/$(uname -r)/build のカーネルヘッダが必要
sudo insmod ./gmm_wake.ko
echo build | sudo tee /sys/kernel/debug/gmm_wake_run   # DMA＋ディスクリプタ設定
echo go    | sudo tee /sys/kernel/debug/gmm_wake_run   # GMM演算を発火
sudo cat /sys/kernel/debug/gmm_wake_run                # スコアを読む
sudo rmmod gmm_wake                                    # 完全に元に戻す
```

debugfs(`/sys/kernel/debug/gmm_wake*`):`gmm_wake`(レジスタダンプ)、`_run`
(GMMジョブのbuild/go)、`_gna`(GNA配置デコード)、`_poke`/`_wr`(レジスタ探索/書込)、
`_sweep`(書込可マップ)。`gmmctl` はbuild/go/pokeサイクルの補助。

## 到達したマイルストーン

- [x] `8086:1911` に自作ドライバをbind(世界に既存ドライバ無し)
- [x] チップを起こす(enable 0→1、D0)、BAR0マップ
- [x] BAR0読み書き、4KBレジスタ配置の地図化
- [x] GNAレジスタ配置(STS/CTRL/START)がpre-0.9でも通用することを確認
- [x] GMM_CONFIGがBAR0 `0x00`、ページディレクトリがBAR0 `0x200` にあると特定
- [x] `0x404` = アドレス範囲上限(vamaxaddr)を発見
- [x] GMM演算を完走(`done=1`)、データ経路が機能
- [x] **特徴依存の本物のGMMスコアを計算、CPU参照とビット一致**(鍵:`vlength`≥24、8は予約値)
- [ ] 相場の波の「ウェイクワード」検出器に転用(下記)

## ⚠️ 注意

- *このチップ/ステッピング*(i7-6600U Skylake GMM)向けのリバースエンジニアリング。
  他のパーツではオフセットが異なる可能性。
- 発火は未文書シリコンにCTRL STARTを書く。不正ディスクリプタで**バス停止 → 電源再起動が必要**。
  `build` は先にquiesce(CTRLアボート)する。完走する演算は約10µsで落ち着く。
- ロードでカーネルがtaintされる(out-of-tree・未署名)。すべて `rmmod` / 再起動で可逆。

## 次にやること

このチップの本来の用途は、音声のための常時オン・超低消費電力の**「耳」**。同じパターン——
*ストリームを見張り、スコアリングし、何かが際立ったら人を起こす*——を、音声の代わりに
**相場の値動き**に適用する計画:急変をGMMスコアリングして通知する。(ユーザー空間はRust。)
このチップで動いてこそ意味がある。

全レジスタ配置・ディスクリプタ形式・MMU配置・デバッグの軌跡は
[`docs/REVERSE-ENGINEERING.md`](docs/REVERSE-ENGINEERING.md) を参照。
