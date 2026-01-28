#!/usr/bin/env node

/**
 * Benchmark Comparison Script
 * 
 * Compares benchmark results from current PR to baseline (main branch).
 * Outputs a markdown-formatted comment for the PR.
 */

const fs = require('fs');
const path = require('path');

// Default metrics to compare (can be overridden via config)
// Units: 'gas/s', 'count', 'ns', 's' (seconds)
const DEFAULT_METRICS = [
  // Sequencer metrics
  { key: 'gas/per_second', name: 'Gas/Second', source: 'sequencer', unit: 'gas/s', higherIsBetter: true },
  { key: 'transactions/per_block', name: 'Transactions/Block', source: 'sequencer', unit: 'count', higherIsBetter: true },
  { key: 'latency/get_payload', name: 'Get Payload Latency', source: 'sequencer', unit: 'ns', higherIsBetter: false },
  { key: 'latency/send_txs', name: 'Send Txs Latency', source: 'sequencer', unit: 'ns', higherIsBetter: false },
  { key: 'reth_op_rbuilder_state_root_calculation_duration', name: 'State Root Duration', source: 'sequencer', unit: 's', higherIsBetter: false },
  // Validator metrics
  { key: 'gas/per_second', name: 'Gas/Second', source: 'validator', unit: 'gas/s', higherIsBetter: true },
  { key: 'latency/new_payload', name: 'New Payload Latency', source: 'validator', unit: 'ns', higherIsBetter: false },
  { key: 'reth_sync_execution_execution_duration', name: 'Execution Duration', source: 'validator', unit: 's', higherIsBetter: false },
  { key: 'reth_reth_flashblocks_block_processing_duration', name: 'Flashblock Processing', source: 'validator', unit: 's', higherIsBetter: false },
];

// SI prefixes for formatting
const SI_PREFIXES = [
  { prefix: 'P', value: 1e15 },
  { prefix: 'T', value: 1e12 },
  { prefix: 'G', value: 1e9 },
  { prefix: 'M', value: 1e6 },
  { prefix: 'k', value: 1e3 },
  { prefix: '', value: 1 },
];

// Time units in nanoseconds
const TIME_UNITS = {
  s: 1e9,
  ms: 1e6,
  µs: 1e3,
  ns: 1,
};

/**
 * Load metrics from a benchmark output directory
 */
function loadMetrics(outputDir) {
  const metadataPath = path.join(outputDir, 'metadata.json');
  
  if (!fs.existsSync(metadataPath)) {
    return null;
  }
  
  const metadata = JSON.parse(fs.readFileSync(metadataPath, 'utf8'));
  
  if (!metadata.runs || metadata.runs.length === 0) {
    return null;
  }
  
  const results = [];
  
  for (const run of metadata.runs) {
    const runDir = path.join(outputDir, run.outputDir);
    const sequencerMetricsPath = path.join(runDir, 'metrics-sequencer.json');
    const validatorMetricsPath = path.join(runDir, 'metrics-validator.json');
    
    const result = {
      testName: run.testName,
      testConfig: run.testConfig,
      sequencerMetrics: null,
      validatorMetrics: null,
      summaryMetrics: run.result,
    };
    
    if (fs.existsSync(sequencerMetricsPath)) {
      result.sequencerMetrics = JSON.parse(fs.readFileSync(sequencerMetricsPath, 'utf8'));
    }
    
    if (fs.existsSync(validatorMetricsPath)) {
      result.validatorMetrics = JSON.parse(fs.readFileSync(validatorMetricsPath, 'utf8'));
    }
    
    results.push(result);
  }
  
  return results;
}

/**
 * Calculate average of a metric across all blocks
 */
function calculateAverage(metrics, metricKey) {
  if (!metrics || metrics.length === 0) return null;
  
  const values = metrics
    .map(block => block.ExecutionMetrics?.[metricKey])
    .filter(v => v !== undefined && v !== null);
  
  if (values.length === 0) return null;
  
  return values.reduce((a, b) => a + b, 0) / values.length;
}

/**
 * Format a number with SI prefix
 */
function formatWithPrefix(value, baseUnit, decimalPlaces = 2) {
  if (value === 0) return `0 ${baseUnit}`;
  
  for (const { prefix, value: multiplier } of SI_PREFIXES) {
    if (Math.abs(value) >= multiplier) {
      return `${(value / multiplier).toFixed(decimalPlaces)} ${prefix}${baseUnit}`;
    }
  }
  
  return `${value.toFixed(decimalPlaces)} ${baseUnit}`;
}

/**
 * Format time value (input in nanoseconds or seconds depending on unit)
 */
function formatTime(value, inputUnit) {
  if (value === null || value === undefined) return 'N/A';
  
  // Convert to nanoseconds first
  let valueInNs;
  if (inputUnit === 's') {
    valueInNs = value * 1e9;
  } else if (inputUnit === 'ms') {
    valueInNs = value * 1e6;
  } else if (inputUnit === 'ns') {
    valueInNs = value;
  } else {
    valueInNs = value;
  }
  
  // Find appropriate unit
  if (valueInNs >= TIME_UNITS.s) {
    return `${(valueInNs / TIME_UNITS.s).toFixed(2)} s`;
  } else if (valueInNs >= TIME_UNITS.ms) {
    return `${(valueInNs / TIME_UNITS.ms).toFixed(2)} ms`;
  } else if (valueInNs >= TIME_UNITS.µs) {
    return `${(valueInNs / TIME_UNITS.µs).toFixed(2)} µs`;
  } else {
    return `${valueInNs.toFixed(2)} ns`;
  }
}

/**
 * Format a value based on its unit
 */
function formatValue(value, unit) {
  if (value === null || value === undefined) return 'N/A';
  
  switch (unit) {
    case 'gas/s':
      return formatWithPrefix(value, 'gas/s');
    case 'gas':
      return formatWithPrefix(value, 'gas');
    case 'count':
      return value.toLocaleString();
    case 'ns':
    case 'ms':
    case 's':
      return formatTime(value, unit);
    case 'bytes':
      return formatWithPrefix(value, 'B');
    default:
      if (Math.abs(value) >= 1e6) {
        return formatWithPrefix(value, '');
      }
      return value.toFixed(2);
  }
}

/**
 * Format a number for display (legacy, used in summary)
 */
function formatNumber(value, scale = 1) {
  if (value === null || value === undefined) return 'N/A';
  
  const scaled = value * scale;
  return formatWithPrefix(scaled, '');
}

/**
 * Calculate percentage change
 */
function calculateChange(current, baseline) {
  if (baseline === null || baseline === undefined || baseline === 0) return null;
  if (current === null || current === undefined) return null;
  
  return ((current - baseline) / Math.abs(baseline)) * 100;
}

/**
 * Get emoji for change direction
 */
function getChangeEmoji(change, higherIsBetter) {
  if (change === null) return '';
  
  const threshold = 1; // 1% threshold for significance
  
  if (Math.abs(change) < threshold) return '➡️';
  
  const isImprovement = higherIsBetter ? change > 0 : change < 0;
  
  if (isImprovement) {
    return Math.abs(change) > 10 ? '🚀' : '✅';
  } else {
    return Math.abs(change) > 10 ? '🔴' : '⚠️';
  }
}

/**
 * Generate markdown comparison table
 */
function generateComparisonTable(currentResults, baselineResults, metricsConfig) {
  const rows = [];
  
  for (const metric of metricsConfig) {
    const sourceMetrics = metric.source === 'sequencer' ? 'sequencerMetrics' : 'validatorMetrics';
    
    // Get current value (average across all blocks from first run)
    const currentRun = currentResults?.[0];
    const currentValue = currentRun?.[sourceMetrics] 
      ? calculateAverage(currentRun[sourceMetrics], metric.key)
      : null;
    
    // Get baseline value
    const baselineRun = baselineResults?.[0];
    const baselineValue = baselineRun?.[sourceMetrics]
      ? calculateAverage(baselineRun[sourceMetrics], metric.key)
      : null;
    
    // Skip if neither has data
    if (currentValue === null && baselineValue === null) continue;
    
    const change = calculateChange(currentValue, baselineValue);
    const emoji = getChangeEmoji(change, metric.higherIsBetter);
    
    rows.push({
      source: metric.source,
      name: metric.name,
      current: formatValue(currentValue, metric.unit),
      baseline: baselineValue !== null ? formatValue(baselineValue, metric.unit) : 'N/A',
      change: change !== null ? `${change >= 0 ? '+' : ''}${change.toFixed(1)}%` : 'N/A',
      emoji,
    });
  }
  
  return rows;
}

/**
 * Generate the full markdown comment
 */
function generateMarkdownComment(currentResults, baselineResults, metricsConfig) {
  const hasBaseline = baselineResults && baselineResults.length > 0;
  const hasCurrent = currentResults && currentResults.length > 0;
  
  if (!hasCurrent) {
    return `## Benchmark Results

⚠️ No benchmark results found for this PR.
`;
  }
  
  const currentRun = currentResults[0];
  const testName = currentRun.testName || 'Benchmark';
  const testConfig = currentRun.testConfig || {};
  
  let markdown = `## 📊 Benchmark Results: ${testName}

`;
  
  // Test configuration
  markdown += `<details>
<summary>📋 Test Configuration</summary>

| Setting | Value |
|---------|-------|
`;
  
  for (const [key, value] of Object.entries(testConfig)) {
    markdown += `| ${key} | \`${value}\` |\n`;
  }
  
  markdown += `
</details>

`;
  
  // Comparison table
  if (hasBaseline) {
    markdown += `### Comparison with \`main\`

`;
  } else {
    markdown += `### Results

> ℹ️ No baseline results found on \`main\`. Showing current results only.

`;
  }
  
  const rows = generateComparisonTable(currentResults, baselineResults, metricsConfig);
  
  // Group by source
  const sequencerRows = rows.filter(r => r.source === 'sequencer');
  const validatorRows = rows.filter(r => r.source === 'validator');
  
  if (sequencerRows.length > 0) {
    markdown += `#### Sequencer Metrics

| Metric | Current | ${hasBaseline ? 'Baseline | Change |' : ''}
|--------|---------|${hasBaseline ? '---------|--------|' : ''}
`;
    
    for (const row of sequencerRows) {
      if (hasBaseline) {
        markdown += `| ${row.name} | ${row.current} | ${row.baseline} | ${row.emoji} ${row.change} |\n`;
      } else {
        markdown += `| ${row.name} | ${row.current} |\n`;
      }
    }
    
    markdown += '\n';
  }
  
  if (validatorRows.length > 0) {
    markdown += `#### Validator Metrics

| Metric | Current | ${hasBaseline ? 'Baseline | Change |' : ''}
|--------|---------|${hasBaseline ? '---------|--------|' : ''}
`;
    
    for (const row of validatorRows) {
      if (hasBaseline) {
        markdown += `| ${row.name} | ${row.current} | ${row.baseline} | ${row.emoji} ${row.change} |\n`;
      } else {
        markdown += `| ${row.name} | ${row.current} |\n`;
      }
    }
    
    markdown += '\n';
  }
  
  // Summary from metadata
  if (currentRun.summaryMetrics) {
    const seqMetrics = currentRun.summaryMetrics.sequencerMetrics || {};
    const valMetrics = currentRun.summaryMetrics.validatorMetrics || {};
    
    markdown += `<details>
<summary>📈 Summary Metrics</summary>

| Metric | Sequencer | Validator |
|--------|-----------|-----------|
| Gas/Second | ${formatValue(seqMetrics.gasPerSecond, 'gas/s')} | ${formatValue(valMetrics.gasPerSecond, 'gas/s')} |
| Fork Choice Updated | ${formatValue(seqMetrics.forkChoiceUpdated, 's')} | - |
| Get Payload | ${formatValue(seqMetrics.getPayload, 's')} | - |
| New Payload | - | ${formatValue(valMetrics.newPayload, 's')} |

</details>

`;
  }
  
  // Legend
  markdown += `<details>
<summary>📖 Legend</summary>

- 🚀 Significant improvement (>10%)
- ✅ Improvement
- ➡️ No significant change (<1%)
- ⚠️ Regression
- 🔴 Significant regression (>10%)

</details>
`;
  
  return markdown;
}

/**
 * Main entry point
 */
function main() {
  const args = process.argv.slice(2);
  
  // Parse arguments
  let currentDir = null;
  let baselineDir = null;
  let outputFile = null;
  let configFile = null;
  
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--current' && args[i + 1]) {
      currentDir = args[++i];
    } else if (args[i] === '--baseline' && args[i + 1]) {
      baselineDir = args[++i];
    } else if (args[i] === '--output' && args[i + 1]) {
      outputFile = args[++i];
    } else if (args[i] === '--config' && args[i + 1]) {
      configFile = args[++i];
    }
  }
  
  if (!currentDir) {
    console.error('Usage: compare-benchmarks.js --current <dir> [--baseline <dir>] [--output <file>] [--config <file>]');
    process.exit(1);
  }
  
  // Load metrics config if provided
  let metricsConfig = DEFAULT_METRICS;
  if (configFile && fs.existsSync(configFile)) {
    const config = JSON.parse(fs.readFileSync(configFile, 'utf8'));
    if (config.metrics) {
      metricsConfig = config.metrics;
    }
  }
  
  // Load results
  const currentResults = loadMetrics(currentDir);
  const baselineResults = baselineDir ? loadMetrics(baselineDir) : null;
  
  // Generate markdown
  const markdown = generateMarkdownComment(currentResults, baselineResults, metricsConfig);
  
  // Output
  if (outputFile) {
    fs.writeFileSync(outputFile, markdown);
    console.log(`Wrote comparison to ${outputFile}`);
  } else {
    console.log(markdown);
  }
}

main();
