package worker

import (
	"sync/atomic"
	"testing"
	"time"
)

func TestPoolRunsEveryJob(t *testing.T) {
	p := NewPool(4)
	var ran atomic.Int32
	for range 10 {
		p.Submit(func() { ran.Add(1) })
	}
	p.Close()
	waitForJobs()
	if got := ran.Load(); got != 10 {
		t.Fatalf("ran %d jobs, want 10", got)
	}
}

func TestPoolDrainsOnClose(t *testing.T) {
	p := NewPool(2)
	var ran atomic.Int32
	p.Submit(func() { ran.Add(1) })
	p.Close()
	<-p.Done()
	if got := ran.Load(); got != 1 {
		t.Fatalf("ran %d jobs, want 1", got)
	}
}

func TestNewPool(t *testing.T) {
	p := NewPool(1)
	if p == nil {
		t.Fatal("NewPool returned nil")
	}
	p.Close()
}

// waitForJobs gives the workers time to finish.
func waitForJobs() { time.Sleep(100 * time.Millisecond) }
