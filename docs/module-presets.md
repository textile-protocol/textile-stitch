# Module strategy presets

Parameters offers three starting points for each quote module: **Competitive**, **Balanced** and **Defensive**. Choose them independently. Inventory balancing controls how much corridor currency to hold; dynamic spreads controls the extra price-movement buffer. There is no dealer or spot-rebalancing preset.

Selection only updates the draft. The sliders and example respond immediately; **Test on history** evaluates that draft. Save remains a separate action. No profile is automatically selected on upgrade, and none is presented as a best-performing strategy.

## Settings and tradeoffs

| Inventory balancing | Competitive | Balanced | Defensive |
| --- | ---: | ---: | ---: |
| Corridor share target | 50% | 30% | 20% |
| Pause purchases at | 75% | 60% | 40% |
| Maximum inventory margin adjustment | 0.10% | 0.15% | 0.25% |

Competitive leaves more currency available for customer trades, accepting more FX exposure. Defensive holds a smaller target, skews quotes more strongly and pauses new purchases sooner. It still encourages buying when below target. It cannot force a sale or protect existing holdings against a devaluation while waiting for buyers. The purchase threshold is not a guarantee that the portfolio share never exceeds it, for example after a withdrawal of settlement currency.

The share targets and limits are explicit risk-budget choices, **not estimates from past returns**. The maximum skew settings give a gentle, intermediate or stronger incentive to rebalance. They are not derived from an estimated customer demand curve. The existing minimum margin is preserved, so a larger skew may reach that floor without tightening sales further. Operators should include costs when choosing the floor; keeping it does not prove it covers them.

| Dynamic spreads | Competitive | Balanced | Defensive |
| --- | ---: | ---: | ---: |
| Price history window | 5 minutes | 15 minutes | 30 minutes |
| Price-range multiplier | 0.5× | 1× | 1.5× |
| Maximum extra margin | 0.05% | 0.15% | 0.25% |
| Warmup | 2 minutes | 2 minutes | 2 minutes |
| Favor inventory reduction | On | On | On |

Competitive responds to shorter-lived movement with a smaller buffer. Defensive retains price shocks longer and applies a larger buffer; this can make quotes less competitive and lose trades. Inventory weighting reduces the addition on the side that moves holdings toward target, reaching zero at the inventory extremes. It needs inventory balancing enabled; otherwise the runtime uses equal additions. Neither the preset nor its name indicates protection against all price moves, feed lag or settlement risk.

Mode, module enable switches, base margins, the inventory floor, and all spot/dealer settings are preserved. An inventory preset is unavailable if it conflicts with an enabled spot-sale threshold; the panel explains the required interval. Disabled spot settings are left alone and must be reconciled before enabling that module. The preset badge is derived from its owned numeric/boolean fields, so reloading retains the label and editing one of those fields shows **Custom settings**. A custom floor does not change the preset label because presets do not own it.

## Professional strategy basis

[Avellaneda and Stoikov, *High-frequency trading in a limit order book* (2008), sections 2.2 and 3](https://math.nyu.edu/~avellane/HighFrequencyTrading.pdf), models the tradeoff between inventory risk and quote execution. Higher inventory changes the dealer's reservation price; volatility and risk aversion affect the quotes. It is an academic limit-order-book model, not a cNGN or FX calibration. Stitch uses a simpler linear inventory skew and a rolling high/low range, not that paper's optimal-control formula, estimated fill intensity or volatility estimator.

[Schrimpf and Sushko, BIS, *FX trade execution: highly complex and fragmented* (December 2019)](https://www.bis.org/publ/qtrpdf/r_qt1912g.htm), documents professional FX dealers warehousing inventory until opposing customer flow offsets it. It also describes how higher volatility makes internalisation harder and can require external hedging. That supports the inventory-versus-competitiveness tradeoff and the need for an eventual executable hedge. It does not prescribe the percentages in these presets.

## Real cNGN/USDT feed check

Retrieved on **7 October 2026** from Textile's public `stitchSimulationHistory` API for the user's BSC vault. The price source was **Textile cNGN feed center (cngn-usdt pricing samples): USDT per cNGN**, matching the canonical live feed. These are recorded source observations, not generated example data or raw Monierate prices.

The requested interval was **6 October 2026 08:52:53 UTC through 7 October 2026 08:52:53 UTC**, Unix seconds `1791276773` through `1791363173`. The response contained 1,500 price observations, including an hour of prehistory; 1,440 were available inside the requested day. There were only **five vault trades**. The median source update interval was 60 seconds, the 95th percentile 67 seconds, and the largest gap 208 seconds. Stored publication/ingestion delay had a median of 2 seconds and maximum of 5 seconds; that does not measure end-to-end bot latency.

| Observed rolling price range | 5 minutes | 15 minutes | 30 minutes |
| --- | ---: | ---: | ---: |
| Median | 0.0037% | 0.0227% | 0.0374% |
| 95th percentile | 0.0432% | 0.0791% | 0.1034% |
| Maximum | 0.1101% | 0.1321% | 0.1336% |

For each in-period observation at availability time `at`, take only observations already available with `at - window <= price_at <= at`, then compute `(max(price) / min(price) - 1) * 10000` in bps. Percentiles use nearest rank, one value per observation, not per second or trade. Prehistory avoids introducing an artificial empty window at the beginning. This is an observation-time range check, not a replay of the bot's evaluation schedule or freshness vetoes.

This cadence supports minute-scale windows and a two-minute source-history warmup, rather than a window shorter than typical source updates. Warmup still depends on actual source timestamps and may take longer when observations are missing. Caps are rounded policy choices checked against this day, not fitted quantiles: Competitive's 5 bps cap clipped 7 of 1,440 uncapped additions; Balanced's largest uncapped addition was 14 bps against its 15 bps cap; Defensive's was 21 bps against 25 bps. Caps deliberately limit the margin and can be exceeded by the underlying market movement.

cNGN's recorded settlement value fell approximately 0.0646% over this sample. One relatively quiet day and five executions cannot establish a profitable preset, calibrate demand, or represent a currency shock. A five-day collection attempt could not read the initial vault snapshot, so no five-day evidence is claimed. These are shared starting profiles; **other corridors have not been calibrated by this check**. Test the actual vault across multiple periods and cost assumptions. Historical replay conditions on observed trades and cannot recover missing customer demand at different quotes.

To reproduce, POST this read-only query to `https://api.textilecredit.com/graphql`, supplying the BSC vault address and the timestamps above as variables:

```graphql
query PresetEvidence($address: String!, $from: Int!, $to: Int!) {
  stitchSimulationHistory(chainId: 56, address: $address, from: $from, to: $to)
}
```

Use the returned `prices` and the rolling-range method above. API retention and archive-RPC availability can affect later reproduction. The panel includes the dated reference check in a collapsed explanation; it is not displayed as a live market reading, and it never silently retunes a saved preset.
