const jhaConfig = require('eslint-config-jha-react-node/node');

module.exports = [
  // target/ is Cargo's build output (git-ignored); `cargo doc` writes rustdoc JavaScript there.
  {ignores: ['tmp/', 'dist/', 'target/', '**/.claude/worktrees/']},
  ...jhaConfig,
];
