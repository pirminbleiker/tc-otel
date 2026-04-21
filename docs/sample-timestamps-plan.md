# Per-Sample-Timestamps im FB_Metrics-Wire-Format

## Motivation

Im bisherigen Aggregate-Wire-Format (`event_type = 21`, Version 2) tragen
Einzel-Samples im Body **keinen eigenen Zeitstempel**. Der Empfänger
interpoliert linear zwischen `dc_time_start` und `dc_time_end`. Das ist korrekt
bei gleichmäßiger Abtastung (z.B. Observe pro Zyklus), verfälscht aber die
zeitliche Darstellung, wenn:

- Change-Detect Bursts produziert (viele `Observe`-Aufrufe im ersten Teil des
  Fensters, dann Ruhe),
- `SetSampleIntervalMs` nicht gesetzt ist und Observe sporadisch aufgerufen
  wird,
- NumericAggregated mehrere Snapshots pro Fenster erzeugt (wobei hier
  Interpolation ohnehin sinnvoll wäre).

Ziel: echter Per-Sample-Zeitstempel **ohne** das Paket unnötig aufzublähen.

## Designentscheidungen

| Entscheidung | Wert |
|---|---|
| Encoding | `u16` Cycle-Offset relativ zu `cycle_count_start`, little-endian |
| Aktivierung | Neuer Flag-Bit `METRIC_FLAG_HAS_SAMPLE_TS = 0x04` im Header-`flags` |
| Body-Layout bei Flag=1 | `[u16 cycle_offset | sample_size B value]` × N, Stride = `sample_size + 2` |
| `sample_size`-Semantik | Unverändert — meint nur die Wert-Bytes, nicht den Offset-Präfix |
| Opt-In-API | `FB_Metrics.SetRecordSampleTimes(bEnable : BOOL) : BOOL` |
| Autoflush-Trigger | In `_CheckFlush`: `nMaxCycles := MIN(target, 16#FFFF)` wenn Flag aktiv |
| Wire-Version | Bleibt 2, nur Flag-Bit additiv — keine Breaking-Change |
| Fallback | Flag=0 → heutige lineare Interpolation 1:1 |

**Warum u16?** Bei 250 µs als minimalem PLC-Tick deckt u16 Push-Fenster bis
16,38 s ab — weit über jedem realistischen Metrik-Intervall. Bei 1-ms-Task
sogar 65,5 s. Der Cycle-Counter ist zudem die Uhr, die FB_Metrics intern
sowieso benutzt.

**Warum u32-Wrap kein Problem ist:** Die PLC-seitige `_CheckFlush`-Logik
verwirft das aktive Fenster am CycleCount-Wrap (`nNowCycle <
_nWindowStartCycle`). Damit straddled kein Fenster die u32-Grenze, und
`CurrentCycle - _nCycleStart` in UDINT-Arithmetik bleibt immer klein und
korrekt.

**Warum Autoflush?** Bei extremen Configs (z.B. 20-s-Push-Intervall auf
250-µs-Task) überschreitet das Fenster die u16-Grenze. Statt einen Offset
überlaufen zu lassen, flusht FB_Metrics vorzeitig. Einzige sichtbare
Konsequenz: `WithSpan`-Pins werden ggf. früher konsumiert — für langlebige
Spans wird `BindTracer` empfohlen.

## Phasen

### Phase 1 — Wire-Format-Spec aktualisieren

**Dateien:**
- `source/.../ST_PushMetricAggHeader.TcDUT` — Kommentar an `flags`: `bit2 =
  has_sample_ts`, neuer Abschnitt für bedingten Body-Layout.
- `docs/push-metrics-wire-format.md` — Body-Section um Sample-TS-Variante
  erweitern, Kostentabelle ergänzen.

### Phase 2 — PLC-Seite (FB_Metrics)

**Datei:** `source/.../FB_Metrics.TcPOU`

- Neues State-Feld `_bRecordSampleTimes : BOOL` (default FALSE).
- Neue Methode `SetRecordSampleTimes(bEnable : BOOL) : BOOL`.
- `_AppendSample` + `_AppendAggregated` akzeptieren optionalen
  `nCycleOff : UINT` und schreiben 2-B-Präfix wenn Flag aktiv.
- `Observe` setzt `_nCycleStart` vor dem Append (statt danach), berechnet
  `nCycleOff := TO_UINT(nNowCycle - _nCycleStart)` und gibt ihn durch.
- `_CheckFlush` cappt `nTargetCycles` auf `16#FFFF` bei aktivem Flag.
- `_DoFlush` OR'd `METRIC_FLAG_HAS_SAMPLE_TS` ins `flags`-Byte.
- Body-Overflow-Check (`BODY_CAPACITY`) berücksichtigt effektiven Stride.

### Phase 3 — Rust-Decoder

**Datei:** `crates/tc-otel-ads/src/diagnostics.rs`

- Neue Konstante `METRIC_FLAG_HAS_SAMPLE_TS: u8 = 1 << 2`.
- `MetricAggregateBatch`-Variant um `sample_cycle_offsets: Option<Vec<u16>>`
  erweitern.

**Datei:** `crates/tc-otel-ads/src/diagnostics_push.rs`

- In `decode_metric_aggregate`: Flag lesen, effektiven Stride berechnen,
  pro Sample 2-B-Offset parsen, parallele `Vec<u16>` aufbauen.
- Truncated-Body-Guard berücksichtigt `sample_size + 2` pro Slot.

### Phase 4 — Bridge (Timestamp-Logik)

**Datei:** `crates/tc-otel-service/src/diagnostics_bridge.rs`

- Signatur `metric_aggregate_to_entries` um `cycle_count_start`,
  `cycle_count_end`, `sample_cycle_offsets: Option<&[u16]>` erweitern.
- Timestamp-Berechnung: `cycle_time_ns = (dc_time_end - dc_time_start) /
  (cycle_count_end - cycle_count_start)` defensiv, danach
  `ts = dc_time_start + offset × cycle_time_ns` wenn Offsets vorhanden,
  sonst bisherige lineare Interpolation.

### Phase 5 — Tests

- **Rust-Unit (decoder):** Roundtrip mit gesetztem Flag + bekannten Offsets;
  Backward-Compat (Flag=0 → Offsets = None); truncated-body wenn Flag gesetzt
  aber Body zu kurz.
- **Rust-Unit (bridge):** Offsets → korrekte Timestamps; Fallback bei
  `cycles == 0`; Fallback bei Offsets-None.
- **Integration:** End-to-end synthetisches Frame → decode → bridge →
  `MetricEntry.timestamp` korrekt.

## Risiken

| Risiko | Mitigation |
|---|---|
| Body-Overflow-Check übersieht effektiven Stride | Guard auf `_nSampleSize + 2` |
| `WithSpan` wird früher konsumiert bei Autoflush | Doc-Hinweis + `BindTracer`-Empfehlung |
| Welford-Split bei Autoflush in Aggregations-Mode | Doc-Hinweis; seltene Nische |
| Korrupte Frames (Flag gesetzt, Body zu kurz) | Decoder-Guard via `checked_mul` |

## Kostenüberblick

| Body-Schema | Heute | Mit HAS_SAMPLE_TS | Overhead |
|---|---|---|---|
| Bool (1 B) | 1 B | 3 B | +200 % |
| Numeric (8 B) | 8 B | 10 B | +25 % |
| NumericAggregated Vollmaske (48 B) | 48 B | 50 B | +4 % |

Nur zahlend, wenn aktiv aufgeschaltet.
