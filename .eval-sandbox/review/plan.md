# Review Plan

## Step 1: Primary Pass (COMPLETED)
- Read all source files (Rust, Swift, Kotlin, config)
- Run tests and clippy
- Identify highest-risk areas
- Write initial findings

## Step 2: Deep Analysis — Android Foreground Service Compatibility
Verify Android 14+ foreground service type requirements:
1. Check AndroidManifest.xml for proper `foregroundServiceType` declaration
2. Verify `startForeground()` call includes the service type parameter
3. Verify `stopForeground()` is called in all stop paths
4. Research best practices for Android 14+ foreground service API changes
5. Verify notification channel creation is idempotent

## Step 3: Deep Analysis — Runner Race Conditions (if needed)
Verify generation counter correctness under rapid stop→start→stop→start:
1. Can the generation counter overflow? (AtomicU64 — practically no)
2. Can stop() race with the spawned task's token cleanup?
3. Is the on_complete callback always called exactly once per task lifecycle?

## Final Step: Synthesis
- Merge all findings into final report
- Approve or request changes
