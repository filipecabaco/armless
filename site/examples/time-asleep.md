---
title: Restored processes keep their deadlines
description: Save a running process, come back later, and restore it told how long it slept. Its timers come due when they would have, instead of picking up where they stopped.
script: sleep.js
side: sleep
pane: sh, user mode
caption: sh runs in user mode with no kernel, from a filesystem with busybox. The snapshot is kept in this browser (IndexedDB), so you can close the tab and come back.
---

## Try it

The panel beside the console takes you through it:

1. The page starts a 60-second timer in the console: a background `sleep 60` that prints when it
   was set and when it fired.
2. **Save and stop** saves the machine and the process, timer included, into this browser.
3. Restore it, now or after closing the tab and coming back:
   - **told how long it was saved**: the timer fires when it was due, at once if that time has
     gone by.
   - **told nothing**: the timer fires as long after the restore as it had left when it was
     saved, as if the process had never stopped.

Each button says beforehand when the timer will fire, and the timer's own line in the console says
when it did. You can restore the same snapshot as many times as you like, either way.

## What is running

This is user mode: there is no guest kernel. The CPU runs busybox's `sh` at EL0, and armless
answers its system calls, `nanosleep` and `wait4` included. A snapshot of a user-mode program is
two pieces: the machine (`save_state()`) and the process (`save_process()`).

On restore, `user_asleep_ms` says how long the process was saved. Its monotonic and boot-time
clocks read that much later, and every wait, interval timer, POSIX timer and timerfd that came
due in that time is due at once. A repeating timerfd counts the periods it missed. The wall clock
is the host's either way.

## The code

```js
// save
await vm.stop();
const state = await vm.save_state();
const process = await vm.save_process();
const savedAt = Date.now();

// later: restore, saying how long it was saved for (0, the default, says no time passed)
await vm.restore_state(state, { user_state: process, user_asleep_ms: Date.now() - savedAt });
vm.run();
```

The page's source is
[site/static/sleep.js](https://github.com/filipecabaco/armless/blob/main/site/static/sleep.js).
