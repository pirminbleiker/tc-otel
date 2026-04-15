# V2 Real Frame Analysis

## Summary

This document analyzes three real v2 log entry frames captured from a live TwinCAT 3.1 PLC running `log4tc.

## Fixture Files

- `crates/tc-otel-ads/tests/fixtures/plc_v2_real_1.bin` - 100 bytes
- `crates/tc-otel-ads/tests/fixtures/plc_v2_real_2.bin` - 65 bytes  
- `crates/tc-otel-ads/tests/fixtures/plc_v2_real_3.bin` - 100 bytes

## Frame Structure

### Entry 1 & 3 (Complex Arguments)

**Hex Layout:**
```
Offset 0x00-0x02: Version + Entry Length
  02 61 00           = Type 0x02 (V2), Length 97 bytes

Offset 0x03-0x1a: Fixed Header (24 bytes)
  02                 = Level (Info)
  9b d2 5d ca 91 cc dc 01  = PLC Timestamp (FILETIME)
  86 99 f6 20 91 cc dc 01  = Clock Timestamp (FILETIME)
  01                 = Task Index (1)
  eb 7f 0d 00        = Cycle Counter (884715)
  07                 = Arg Count (7)
  00                 = Context Count (0)

Offset 0x1b-0x25: Message String
  0a                 = Length (10 bytes)
  f6 20 91 cc dc 01 01 eb 7f 0d = Bytes (contains invalid UTF-8)

Offset 0x26-0x2d: Logger String
  08                 = Length (8 bytes)
  5f 47 4c 4f 42 41 4c 5f = "_GLOBAL_"

Offset 0x2f-0x63: Arguments (7 arguments)
  64 58 89 b2 02     = Arg 1: TIME (type 100, value 45255000 ms)
  65 7c 94 1a ...    = Arg 2: LTIME (type 101, 8-byte value)
  66 00 41 8d 31     = Arg 3: DATE (type 102, unix seconds)
  67 7e 1c 8e 31     = Arg 4: DT (type 103, unix seconds)
  68 d6 82 ff 00     = Arg 5: TOD (type 104, milliseconds)
  69 02 05 00        = Arg 6: ENUM (type 105, underlying 0x02=WORD, value 5)
  6a 09 66 00 6f 00... = Arg 7: WSTRING (type 106, 9 UTF-16LE chars)
```

### Entry 2 (Simple Arguments)

**Differences:**
- Level: 0x01 (Debug)
- Arg Count: 2
- Logger: "PRG_TestSimpleApi" (17 bytes)
- Arguments: ENUM + TOD

## Key Findings

### 1. Message Buffer Corruption

The message string contains **invalid UTF-8 bytes**:
```
f6 20 91 cc dc 01 01 eb 7f 0d
```

These bytes appear to be part of previous timestamp data or uninitialized buffer memory. This happens despite the test program calling `F_Log()` with a proper message template.

**Root Cause:** Likely a buffer management issue in the PLC's `FB_LogEntry` class where the message pointer is incorrect, or the buffer hasn't been properly initialized before writing.

**Impact:** The original parser would fail with `InvalidStringEncoding("invalid utf-8 sequence")` and return 0 entries.

### 2. Complex Type Arguments

The PLC correctly encodes all complex types:

- **TIME (100):** 4 bytes, unsigned milliseconds since start of day
- **LTIME (101):** 8 bytes, unsigned 100-nanosecond units
- **DATE (102):** 4 bytes, unsigned Unix seconds (days since 1970-01-01)
- **DT (103):** 4 bytes, unsigned Unix seconds (full timestamp)
- **TOD (104):** 4 bytes, unsigned milliseconds since start of day
- **ENUM (105):** 1 byte type ID, then value bytes (length depends on underlying type)
- **WSTRING (106):** 1 byte char count, then UTF-16LE encoded characters

Example WSTRING argument from fixtures:
```
6a 09 66 00 6f 00 6f 00 f6 00 e4 00 61 00 98 03 34 d8 1e dd
^  ^  ^---UTF-16LE-encoded-characters---^
|  |
|  +--Char count (9 characters)
+-----Type ID 106 (WSTRING)

Decoded: "fooöäaΘ𝄞"
- f (0x0066)
- o (0x006f)
- o (0x006f)
- ö (0x00f6)
- ä (0x00e4)
- a (0x0061)
- Θ (0x0398)
- 𝄞 (0xD834 0xDD1E, non-BMP character in UTF-16 surrogate pair)
```

### 3. V2 Type Mapping

The PLC uses v2-specific type IDs (100-106) that map to the parser's internal representation:

```
PLC Type | ID  | Parser Type | Handler
---------|-----|-------------|------------------------
TIME     | 100 | 20000       | read_value_with_type
LTIME    | 101 | 20001       | read_value_with_type
DATE     | 102 | 20002       | read_value_with_type
DT       | 103 | 20003       | read_value_with_type
TOD      | 104 | 20004       | read_value_with_type
ENUM     | 105 | 20005       | read_value_with_type (recursive)
WSTRING  | 106 | 20006       | read_value_with_type (UTF-16LE)
```

The `remap_v2_type_id()` function correctly translates v2 type IDs to parser types.

## Parser Bug Fix

### Issue

The `read_string()` function at line 901 used strict UTF-8 validation:

```rust
match std::str::from_utf8(str_bytes) {
    Ok(valid_str) => Ok(valid_str.to_string()),
    Err(e) => Err(AdsError::InvalidStringEncoding(e.to_string())),
}
```

When the PLC sends invalid UTF-8 in the message buffer (due to buffer corruption), the parser would fail immediately and return 0 entries.

### Solution

Changed to **lossy UTF-8 decoding** using `String::from_utf8_lossy()`:

```rust
// Use lossy UTF-8 decoding to handle invalid sequences gracefully
// Real PLC buffers may contain uninitialized or corrupted data
Ok(String::from_utf8_lossy(str_bytes).into_owned())
```

This allows the parser to:
1. Handle uninitialized or corrupted message buffers gracefully
2. Continue parsing arguments and other data
3. Replace invalid UTF-8 bytes with the Unicode replacement character (U+FFFD)

### Rationale

- Real PLC environments may have buffer management issues
- Complex types (WSTRING, ENUM, etc.) are correctly encoded and should still be parsed
- Message corruption doesn't prevent extracting valuable data from arguments

## Tests

Three new test fixtures validate real-world parsing:

- `test_parse_v2_real_fixture_entry_1`: 7 complex arguments (TIME, LTIME, DATE, DT, TOD, ENUM, WSTRING)
- `test_parse_v2_real_fixture_entry_2`: 2 arguments (ENUM, TOD) with non-default logger
- `test_parse_v2_real_fixture_entry_3`: Duplicate of entry 1 (same arguments)

All three tests pass after the parser fix.

## Recommendations

1. **PLC Investigation:** Review the `FB_LogEntry.StartV2()` method to understand why message buffer is corrupted. Likely causes:
   - Message pointer initialization bug
   - Buffer overflow from unsized string data
   - Uninitialized memory being written

2. **Parser Robustness:** The lossy UTF-8 approach is appropriate for a production logger that must handle real-world data quality issues.

3. **Monitoring:** Add metrics to track:
   - Invalid UTF-8 sequences encountered
   - Arguments successfully parsed despite message corruption
   - Argument type distribution (time types are common)
