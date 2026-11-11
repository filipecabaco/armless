// The probe the user-mode experiment runs: a static linux/arm64 Go binary that prints, spins up
// goroutines across threads (clone, futex, the netpoller), does a fixed CPU loop, and prints a
// checksum, so a wrong answer is visible and timing comparable.
//
// With `hold` it then stays up: each line on stdin runs the same work again and prints its line
// (a command to a restored user-mode site, which has no shell), and at the end of stdin it sleeps.
package main

import (
	"bufio"
	"fmt"
	"os"
	"runtime"
	"sync"
	"time"
)

func work(seed uint64, n int) uint64 {
	x := seed
	for i := 0; i < n; i++ {
		x ^= x << 13
		x ^= x >> 7
		x ^= x << 17
	}
	return x
}

func round() {
	start := time.Now()
	const workers = 8
	const iters = 2_000_000
	var wg sync.WaitGroup
	results := make([]uint64, workers)
	ch := make(chan int, workers)
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			results[w] = work(uint64(w+1)*0x9e3779b97f4a7c15, iters)
			ch <- w
		}(w)
	}
	wg.Wait()
	close(ch)
	done := 0
	for range ch {
		done++
	}

	var sum uint64
	for _, r := range results {
		sum ^= r
	}
	// Some allocation, so the GC runs at least once
	var keep [][]byte
	for i := 0; i < 2000; i++ {
		keep = append(keep, make([]byte, 4096))
		if len(keep) > 100 {
			keep = keep[1:]
		}
	}
	runtime.GC()

	fmt.Printf("probe: workers=%d done=%d sum=%016x gomaxprocs=%d elapsed=%s\n",
		workers, done, sum, runtime.GOMAXPROCS(0), time.Since(start))
}

func main() {
	fmt.Println("probe: hello")
	round()
	if len(os.Args) > 1 && os.Args[1] == "hold" {
		in := bufio.NewScanner(os.Stdin)
		for in.Scan() {
			round()
		}
		// A bare select{} is a deadlock to the Go runtime; a sleeping timer is not
		for {
			time.Sleep(time.Hour)
		}
	}
	os.Exit(0)
}
