{ pkgs, ... }:
{
  packages = with pkgs; [
    cloud-hypervisor
    virtiofsd
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
  };

  enterShell = ''
    echo "🚀 agentvm devenv active! Build VM with: make vm"
  '';
}
