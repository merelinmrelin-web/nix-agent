# nix-agent plan v1
# plan-id: 2026-06-29-ripgrep-fd
# prompt: install ripgrep and fd

{
  environment.systemPackages = with pkgs; [
    ripgrep
    fd
  ];
}
