# Review Plan: Background Service Plugin Runtime Verification

## Step 1: Primary Pass (CURRENT — COMPLETED)
- Rebuild APK with latest code (13 commits since last test)
- Deploy to Waydroid
- Run full 10-test AutoGLM suite
- Manual verification of race condition, notification
- Result: 5/5 core PASS, 2/2 lifecycle PASS, 3 edge INFO
- Key finding: LifecycleService not visible in dumpsys

## Step 2: Deep Analysis — Foreground Service Lifecycle (NEXT)
Investigate why LifecycleService does not appear in dumpsys:
1. Check if Waydroid suppresses foreground service dumpsys output
2. Add temporary logging to LifecycleService and rebuild
3. Start service, check logcat for onStartCommand execution
4. Verify startForeground() is called within the 5-second window
5. Check if the notification channel is created and notification posted
6. Test OS kill resilience: background the app and check if service survives
7. Document whether this is a real bug or Waydroid testing limitation

## Final Step: Synthesis and Completion
- Consolidate findings from Steps 1-2
- Produce final review report with verdict
- If foreground service issue is confirmed: REQUEST_CHANGES
- If foreground service issue is Waydroid-only: APPROVE with documented caveat
