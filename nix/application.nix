{ pkgs, runtime }:

{ images }:

let
  inherit (pkgs) lib;
  validName = name:
    name != ""
    && !(lib.hasInfix "/" name)
    && name != "."
    && name != "..";
  invalidNames = builtins.filter (name: !(validName name)) (builtins.attrNames images);
  normaliseLayer = layer: {
    source = layer.source;
    target = layer.target or ".";
    rules = layer.rules or "";
  };
  normaliseImage = image: {
    root = image.root or "~";
    layers = map normaliseLayer image.layers;
  };
  manifest = pkgs.writeText "tohru-manifest.json" (builtins.toJSON {
    images = builtins.mapAttrs (_: normaliseImage) images;
  });
  launcher = pkgs.writeShellScript "tohru" ''
    exec ${runtime}/bin/tohru --manifest ${manifest} "$@"
  '';
in
assert lib.assertMsg (invalidNames == [ ])
  "tohru image names must be normal path components";
{
  type = "app";
  program = toString launcher;
}
