#!/usr/bin/perl
# Move the package versions cog bumps. BSD sed rejects `sed -i` and the
# `0,` address the old hooks used, so this is perl, which macOS, Linux
# and Git for Windows all have. The first `version = "..."` line of each
# file moves; a later one does not. Each `rtrash = ` line's pin moves.
# `release = "..."` in conf.py moves; its short `version = ` does not.
use strict;
use warnings;

my $version = shift @ARGV;
die "usage: bump-version.pl VERSION\n" if !defined $version || $version eq "";
die "version is not a single token\n" if $version =~ /[\r\n"\\\/]/;

sub slurp {
    my ($path) = @_;
    open my $in, "<:raw", $path or die "missing $path\n";
    local $/;
    my $text = <$in>;
    close $in;
    return $text;
}

sub store {
    my ($path, $text) = @_;
    open my $out, ">:raw", $path or die "cannot write $path\n";
    print $out $text;
    close $out or die "cannot write $path\n";
}

sub lines_of {
    my ($text) = @_;
    my @lines;
    while ($text =~ /\G([^\n]*\n|[^\n]+)/g) {
        push @lines, $1;
    }
    return @lines;
}

sub rewrite_version {
    my ($path) = @_;
    my $done = 0;
    my $out  = "";
    for my $line (lines_of(slurp($path))) {
        if (!$done && $line =~ /^version = "/) {
            $line =~ s/^version = "[^"]*"/version = "$version"/;
            $done = 1;
        }
        $out .= $line;
    }
    die "no version line in $path\n" unless $done;
    store($path, $out);
}

sub rewrite_pins {
    my ($path) = @_;
    my $out = "";
    for my $line (lines_of(slurp($path))) {
        if ($line =~ /^rtrash = /) {
            $line =~ s/version = "[^"]*"/version = "$version"/;
        }
        $out .= $line;
    }
    store($path, $out);
}

sub rewrite_release {
    my ($path) = @_;
    my $done = 0;
    my $out  = "";
    for my $line (lines_of(slurp($path))) {
        if ($line =~ /^release = "/) {
            $line =~ s/^release = "[^"]*"/release = "$version"/;
            $done = 1;
        }
        $out .= $line;
    }
    die "no release line in $path\n" unless $done;
    store($path, $out);
}

rewrite_version("Cargo.toml");
rewrite_version("pyproject.toml");
rewrite_version("crates/rtrash-hud/Cargo.toml");
rewrite_pins("crates/rtrash-hud/Cargo.toml");
rewrite_release("docs/source/conf.py");
