# gmm_wake

**Making a driverless, forgotten 2016 Intel accelerator compute again — on Linux, from scratch.**

`gmm_wake` is a hand-written Linux kernel module that takes the Intel **Gaussian
Mixture Model (GMM)** accelerator — PCI `8086:1911`, the Skylake-era (pre-0.9)
ancestor of Intel GNA — from *disabled, no driver anywhere on Earth* to
**running a GMM scoring operation to completion with a working data path** (it
reads our mapped input and writes the score back). The feature-dependent
distance arithmetic is **not yet working** — see *Honest status* below.

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

## Honest status

What is **proven to work**:

```
FIRED.
  STS 0x80 after=0x00000001   done=1  err=0x0   (~10 us)
  score @data = 0x000003e8 (1000)  == gconst
```

- The chip is woken, accepts a GMM descriptor, runs the op to completion
  (`done=1`, ~10 µs) with no error bits.
- The **data path works**: the score output tracks `gconst` exactly
  (gconst 1000 → score 1000, gconst 4660 → score 4660). So the chip reads our
  input from mapped memory (through the MMU we built) and writes the output back.

What does **NOT yet work**:

- The **feature-dependent distance term**. With feature far from the mean
  (feature=100, mean=0, variance=255) the score is *still* exactly `gconst`
  (1000), not the `gconst + Σ var·(f−m)²` an L2 GMM score should give. So right
  now the pipeline is effectively a **constant (`gconst`) passthrough**, not a
  meaningful GMM computation. Why the distance accumulation is zero — a wrong
  GMM_CONFIG field, the feature/mean not being read, or a score-output
  semantics detail — is the open problem.

In short: the hard infrastructure (wake, protocol, MMU, addressing, getting a
clean `done=1`) is solved; the actual GMM math is not yet producing a
feature-dependent result.

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

## Milestones

- [x] Bind a custom driver to `8086:1911` (no driver exists anywhere)
- [x] Wake the chip (enable 0→1, D0) and map BAR0
- [x] Read/write BAR0; map the 4 KB register layout
- [x] Confirm the GNA register map (STS/CTRL/START) applies to pre-0.9 silicon
- [x] Locate GMM_CONFIG in BAR0 `0x00` and the page directory in BAR0 `0x200`
- [x] Discover `0x404` = the address-range limit (vamaxaddr)
- [x] Run a GMM scoring op to completion (`done=1`) with a working data path
      (score tracks `gconst`)
- [ ] **Make the feature-dependent distance term actually compute** (score is
      currently a constant `gconst` passthrough)
- [ ] Repurpose as a market-wave "wake-word" detector (see below)

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

## 正直な現在地

**できていること**:

```
FIRED.
  STS 0x80 after=0x00000001  done=1  err=0x0  (~10us)
  score = 0x000003e8 (1000)  == gconst
```

- チップを起こし、GMMディスクリプタを受理、演算を完走(`done=1`, ~10µs、エラー無し)。
- **データ経路は本物**:スコア出力が `gconst` にぴったり追従(gconst 1000→score 1000、
  4660→4660)。= 我々が構築したMMU経由でマップ済みメモリから入力を読み、出力を書き戻している。

**まだできていないこと**:

- **特徴ベクトル依存の距離項**。feature を平均から大きく離しても(feature=100, mean=0,
  variance=255)、スコアは**依然ちょうど `gconst`(1000)**で、L2 GMMスコアの
  `gconst + Σ var·(f−m)²` にならない。つまり今は**定数(`gconst`)のパススルー**であって、
  有意味なGMM計算ではない。距離の積算が0になる理由(GMM_CONFIGのフィールド誤り、
  feature/meanが読まれていない、スコア出力の意味の取り違え)が未解決の課題。

要するに:土台の難所(起動・プロトコル・MMU・アドレッシング・クリーンな `done=1`)は
解けたが、**実際のGMM演算はまだ特徴依存の結果を出していない**。

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
- [x] GMM演算を完走(`done=1`)、データ経路が機能(スコアがgconstに追従)
- [ ] **特徴依存の距離項を実際に計算させる**(現状は定数gconstのパススルー)
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
