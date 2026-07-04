# Task 1 Report: NoteStore 全局编辑锁

## Status
✅ **DONE** - All requirements implemented and tested.

## What Was Implemented

### 1. Global Edit Lock & Guard Function
- Added module-private static `EDIT_LOCK: std::sync::Mutex<()>` to serialize all non-active writer edits
- Added helper function `edit_guard()` that acquires the lock with poison handling via `unwrap_or_else(into_inner)`
- Lock rationale: NoteStore instances are created per-command and stateless; concurrent read-modify-write operations on `speakers.json` and `segments.jsonl` caused lost updates. The lock prevents this by serializing all mutations at the method entry point
- Lock scope: Module-private; zero caller visibility. Active writers (NoteWriter) use their own Mutex; this lock only affects non-active (UI) edits

### 2. Concurrent Test
- Added `concurrent_speaker_edits_do_not_lose_updates` regression test:
  - Creates a note with 2 segments and speaker S1
  - Spawns 2 threads:
    - Thread 1: 20 iterations of `rename_speaker` (modifying S1 name to "名0" through "名19")
    - Thread 2: 20 iterations of `set_segment_speaker(..., "new")` (allocating S2 through S21)
  - Verifies final state:
    - S1 name is "名19" (thread 1's last write survived)
    - Speaker count is 21 (S1 + S2..S21, proving all 20 allocations persisted)
  - Without the lock, either assertion would fail due to simultaneous read-modify-write clobbering

### 3. Lock Integration
Applied `let _guard = edit_guard();` as the first statement in all 6 mutation methods:
1. `rename(&self, id, title)` - writes meta.json
2. `delete(&self, id)` - deletes entire directory
3. `rename_speaker(&self, id, speaker_id, name)` - writes speakers.json
4. `edit_segment_text(&self, id, seq, expected_text, new_text)` - writes segments.jsonl
5. `delete_segment(&self, id, seq, expected_text)` - rewrites segments.jsonl
6. `set_segment_speaker(&self, id, seq, expected_text, speaker_id)` - writes both speakers.json and segments.jsonl

Read-only methods (`load`, `list`) do not acquire the lock; correctness relies on per-write atomicity (tmp+rename pattern).

## TDD Evidence

### RED (Failing Test)
The concurrent test fails without the lock because threads race to read-modify-write shared files:

Without implementing the lock, running the test shows:
```
test store::notes::tests::concurrent_speaker_edits_do_not_lose_updates ... FAILED
thread panicked at src/store/notes.rs:562:93: 
  called `Result::unwrap()` on an `Err` value
```

The race condition causes file access failures as threads clobber each other's reads and writes.

### GREEN (Passing Test)
After adding EDIT_LOCK and guard to all 6 mutation methods:

```bash
cargo test store:: --lib
  → test store::notes::tests::concurrent_speaker_edits_do_not_lose_updates ... ok
  → test result: ok. 41 passed; 0 failed
```

All 41 store tests pass, including the new regression test.

## Files Changed

- **`src-tauri/src/store/notes.rs`**: +59 lines
  - Lines 10-18: Static lock and guard function
  - Lines 81, 89, 97, 115, 128, 146: Guard acquisition in mutation methods
  - Lines 528-569: New concurrent test

## Self-Review Findings

### ✅ Correctness
- Lock acquired at start of every mutation method, before any state read
- Guard dropped at method exit (RAII), ensuring lock release on success or error
- Poison handling matches spec: panicking writer doesn't leave half-written state (each atomic write is independent)
- Test verifies both: final write survived AND all intermediate updates persisted

### ✅ Performance & Scope
- Lock only on mutations; reads (`list`, `load`) remain lock-free
- Edit operations are millisecond-scale and rare; serialization is user-invisible
- Zero API surface change

### ✅ Test Quality
- Regression test catches both lost-update scenarios (final and intermediate)
- Realistic workload (20 concurrent iterations)
- Both main edit patterns tested (speakers and segments)

## Test Results Summary

```
cargo test store:: --lib
  ✅ 41 passed (concurrent_speaker_edits_do_not_lose_updates + 40 existing)
  ✅ 0 failed
  ✅ No new compilation warnings
```

## Commit

```
Commit: 645dc70
Message: fix(store): NoteStore 变更方法加全局编辑锁,根治非活动写者并发丢更新
Files: src-tauri/src/store/notes.rs (+59)
```

---

**Verification:** All 5 task steps completed. Lock is in place, test passes, commit created.
