package proposer

import (
	"testing"

	"github.com/stretchr/testify/assert"
)

// TestCreateSpans tests the CreateSpans function. Confirms that the function creates non-overlapping spans of size MaxBlockRangePerSpanProof
// from the start to end block, and that the spans are contiguous.
func TestCreateSpans(t *testing.T) {
	tests := []struct {
		name               string
		start              uint64
		end                uint64
		maxBlockRange      uint64
		expectedSpansCount int
		expectedFirstSpan  Span
		expectedLastSpan   Span
	}{
		{
			name:               "Single full span",
			start:              100,
			end:                200,
			maxBlockRange:      100,
			expectedSpansCount: 1,
			expectedFirstSpan:  Span{Start: 100, End: 200},
			expectedLastSpan:   Span{Start: 100, End: 200},
		},
		{
			name:               "Multiple full spans",
			start:              1000,
			end:                3000,
			maxBlockRange:      500,
			expectedSpansCount: 3,
			expectedFirstSpan:  Span{Start: 1000, End: 1500},
			expectedLastSpan:   Span{Start: 2000, End: 2500},
		},
		{
			name:               "Partial last span excluded",
			start:              100,
			end:                350,
			maxBlockRange:      100,
			expectedSpansCount: 2,
			expectedFirstSpan:  Span{Start: 100, End: 200},
			expectedLastSpan:   Span{Start: 200, End: 300},
		},
		{
			name:               "No spans possible",
			start:              100,
			end:                150,
			maxBlockRange:      100,
			expectedSpansCount: 0,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			l := &L2OutputSubmitter{}
			l.Cfg = ProposerConfig{MaxBlockRangePerSpanProof: tt.maxBlockRange}

			spans := l.CreateSpans(tt.start, tt.end)

			assert.Equal(t, tt.expectedSpansCount, len(spans), "Unexpected number of spans")

			if tt.expectedSpansCount > 0 {
				assert.Equal(t, tt.expectedFirstSpan, spans[0], "First span mismatch")
				assert.Equal(t, tt.expectedLastSpan, spans[len(spans)-1], "Last span mismatch")
			}

			for i := 0; i < len(spans)-1; i++ {
				assert.Equal(t, spans[i].End, spans[i+1].Start, "Spans should be contiguous")
				assert.Equal(t, tt.maxBlockRange, spans[i].End-spans[i].Start, "Span range should match maxBlockRange")
			}
		})
	}
}
