package worker

import "sync"

// Pool runs jobs on a fixed number of goroutines.
type Pool struct {
	jobs chan func()
	wg   sync.WaitGroup
	done chan struct{}
}

func NewPool(size int) *Pool {
	p := &Pool{jobs: make(chan func()), done: make(chan struct{})}
	for range size {
		p.wg.Add(1)
		go func() {
			defer p.wg.Done()
			for job := range p.jobs {
				job()
			}
		}()
	}
	go func() { p.wg.Wait(); close(p.done) }()
	return p
}

func (p *Pool) Submit(job func()) { p.jobs <- job }

// Close stops accepting jobs; Done closes once every running job returns.
func (p *Pool) Close()                { close(p.jobs) }
func (p *Pool) Done() <-chan struct{} { return p.done }
