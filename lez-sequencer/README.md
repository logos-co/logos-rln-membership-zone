# Your own LEZ sequencer as the block source

Optional. By default the Zone sequencer reads the public LEZ Sequencer (`https://testnet.lez.logos.co`).
A LEZ sequencer run here, without a stake, follows the LEZ channel on Bedrock from your own
node, re-executes it and serves the same JSON-RPC on `127.0.0.1:3040`; point
`LEZ_SEQUENCER_URL` at it.

`sequencer_config.json` is the template: `bedrock_config.node_url` is the local node,
`channel_id` the testnet LEZ channel. The `genesis` section must be the testnet's, from the
LEZ team; the placeholder in the template does not run. Steps, build and verification:
[`docs/setup-rln-membership-zone.md`](../docs/setup-rln-membership-zone.md), section 5. The sequencer home lives in
`home/` (gitignored).
