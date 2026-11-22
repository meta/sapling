# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# helptext.py - static help data for mercurial
#
# Copyright 2006 Mercurial Contributors
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


bundlespec = r"""@Product@ supports generating standalone "bundle" files that hold repository
data. These "bundles" are typically saved locally and used later or exchanged
between different repositories, possibly on different machines. Example
commands using bundles are :prog:`bundle` and :prog:`unbundle`.

Generation of bundle files is controlled by a "bundle specification"
("bundlespec") string. This string tells the bundle generation process how
to create the bundle.

A "bundlespec" string is composed of the following elements:

type
    A string denoting the bundle format to use.

compression
    Denotes the compression engine to use compressing the raw bundle data.

parameters
    Arbitrary key-value parameters to further control bundle generation.

A "bundlespec" string has the following formats:

<type>
    The literal bundle format string is used.

<compression>-<type>
    The compression engine and format are delimited by a hyphen (``-``).

Optional parameters follow the ``<type>``. Parameters are URI escaped
``key=value`` pairs. Each pair is delimited by a semicolon (``;``). The
first parameter begins after a ``;`` immediately following the ``<type>``
value.

Available Types
===============

The following bundle <type> strings are available:

v1
    Produces a legacy "changegroup" version 1 bundle.

    This format is compatible with nearly all @Product@ clients because it is
    the oldest. However, it has some limitations, which is why it is no longer
    the default for new repositories.

    ``v1`` bundles can be used with modern repositories using the "generaldelta"
    storage format. However, it may take longer to produce the bundle and the
    resulting bundle may be significantly larger than a ``v2`` bundle.

    ``v1`` bundles can only use the ``gzip``, ``bzip2``, and ``none`` compression
    formats.

v2
    Produces a version 2 bundle.

    Version 2 bundles are an extensible format that can store additional
    repository data (such as bookmarks and phases information) and they can
    store data more efficiently, resulting in smaller bundles.

    Version 2 bundles can also use modern compression engines, such as
    ``zstd``, making them faster to compress and often smaller.

Available Compression Engines
=============================

The following bundle <compression> engines can be used:

.. bundlecompressionmarker

Examples
========

``v2``
    Produce a ``v2`` bundle using default options, including compression.

``none-v1``
    Produce a ``v1`` bundle with no compression.

``zstd-v2``
    Produce a ``v2`` bundle with zstandard compression using default
    settings.

``zstd-v1``
    This errors because ``zstd`` is not supported for ``v1`` types.
"""


color = r"""@Product@ colorizes output from several commands.

For example, the diff command shows additions in green and deletions
in red, while the status command shows modified files in magenta. Many
other commands have analogous colors. It is possible to customize
these colors.

To enable color (default) whenever possible use::

  [ui]
  color = yes

To disable color use::

  [ui]
  color = no

See :prog:`help config.ui.color` for details.

Mode
====

@Product@ can use various systems to display color. The supported modes are
``ansi``, ``win32``, and ``terminfo``.  See :prog:`help config.color` for details
about how to control the mode.

Effects
========

Other effects in addition to color, like bold and underlined text, are
also available. By default, the terminfo database is used to find the
terminal codes used to change color and effect.  If terminfo is not
available, then effects are rendered with the ECMA-48 SGR control
function (aka ANSI escape codes).

The available effects in terminfo mode are 'blink', 'bold', 'dim',
'inverse', 'invisible', 'italic', 'standout', and 'underline'; in
ECMA-48 mode, the options are 'bold', 'inverse', 'italic', and
'underline'.  How each is rendered depends on the terminal emulator.
Some may not be available for a given terminal type, and will be
silently ignored.

If the terminfo entry for your terminal is missing codes for an effect
or has the wrong codes, you can add or override those codes in your
configuration::

  [color]
  terminfo.dim = \E[2m

where '\E' is substituted with an escape character.

Labels
======

Text receives color effects depending on the labels that it has. Many
default @Product@ commands emit labelled text. You can also define
your own labels in templates using the label function, see :prog:`help
templates`. A single portion of text may have more than one label. In
that case, effects given to the last label will override any other
effects. This includes the special "none" effect, which nullifies
other effects.

Labels are normally invisible. In order to see these labels and their
position in the text, use the global --color=debug option. The same
anchor text may be associated to multiple labels, e.g.

  [log.changeset changeset.secret|changeset:   22611:6f0a53c8f587]

The following are the default effects for some default labels. Default
effects may be overridden from your configuration file::

  [color]
  status.modified = blue bold underline red_background
  status.added = green bold
  status.removed = red bold blue_background
  status.deleted = cyan bold underline
  status.unknown = magenta bold underline
  status.ignored = black bold

  # 'none' turns off all effects
  status.clean = none
  status.copied = none

  qseries.applied = blue bold underline
  qseries.unapplied = black bold
  qseries.missing = red bold

  diff.diffline = bold
  diff.extended = cyan bold
  diff.file_a = red bold
  diff.file_b = green bold
  diff.hunk = magenta
  diff.deleted = red
  diff.inserted = green
  diff.changed = white
  diff.tab =
  diff.trailingwhitespace = bold red_background

  # Blank so it inherits the style of the surrounding label
  changeset.public =
  changeset.draft =
  changeset.secret =

  resolve.unresolved = red bold
  resolve.resolved = green bold

  bookmarks.active = green

  branches.active = none
  branches.closed = black bold
  branches.current = green
  branches.inactive = none

  tags.normal = green
  tags.local = black bold

  rebase.rebased = blue
  rebase.remaining = red bold

  shelve.age = cyan
  shelve.newest = green bold
  shelve.name = blue bold

  histedit.remaining = red bold

Custom colors
=============

Because there are only eight standard colors, @Product@ allows you
to define color names for other color slots which might be available
for your terminal type, assuming terminfo mode.  For instance::

  color.brightblue = 12
  color.pink = 207
  color.orange = 202

to set 'brightblue' to color slot 12 (useful for 16 color terminals
that have brighter colors defined in the upper eight) and, 'pink' and
'orange' to colors in 256-color xterm's default color cube.  These
defined colors may then be used as any of the pre-defined eight,
including appending '_background' to set the background to that color.
"""


common = r""".. Common link and substitution definitions.

.. |hg(1)| replace:: **hg**\ (1)
.. _hg(1): hg.1.html
.. |hgrc(5)| replace:: **hgrc**\ (5)
.. _hgrc(5): hgrc.5.html
"""


config = r"""The @Product@ system uses a set of configuration files to control
aspects of its behavior.

Troubleshooting
===============

If you're having problems with your configuration,
:prog:`config --debug` can help you understand what is introducing
a setting into your environment.

See :prog:`help config.syntax` and :prog:`help config.files`
for information about how and where to override things.

Structure
=========

The configuration files use a simple ini-file format. A configuration
file consists of sections, led by a ``[section]`` header and followed
by ``name = value`` entries::

  [ui]
  username = Firstname Lastname <firstname.lastname@example.net>
  verbose = True

The above entries will be referred to as ``ui.username`` and
``ui.verbose``, respectively. See :prog:`help config.syntax`.

Files
=====

@Product@ reads configuration data from several files, if they exist.
These files do not exist by default and you will have to create the
appropriate configuration files yourself.

The configuration files' locations depend on the current platform. Please
consult :prog:`configfile` to discover your platform's locations.

Per-repository configuration options only apply in a
particular repository. This file is not version-controlled, and
will not get transferred during a "clone" operation. Options in
this file override options in all other configuration files.

Per-user configuration file(s) are for the user running @Product@.  Options
in these files apply to all @Product@ commands executed by this user in any
directory. Options in these files override per-system and per-installation
options.

Per-system configuration files are for the system on which @Product@
is running. Options in these files apply to all @Product@ commands
executed by any user in any directory. Options in these files
override per-installation options.

Warning: Running @prog@ inside, pushing to, pulling from, or cloning local
repositories owned by other users will load the their config files. That could
be potentially harmful. A config file can run arbitrary code by defining
extensions or hooks.

Syntax
======

A configuration file consists of sections, led by a ``[section]`` header
and followed by ``name = value`` entries (sometimes called
``configuration keys``)::

    [spam]
    eggs=ham
    green=
       eggs

Each line contains one entry. If the lines that follow are indented,
they are treated as continuations of that entry. Leading whitespace is
removed from values. Empty lines are skipped. Lines beginning with
``#`` or ``;`` are ignored and may be used to provide comments.

Configuration keys can be set multiple times, in which case @Product@
will use the value that was configured last. As an example::

    [spam]
    eggs=large
    ham=serrano
    eggs=small

This would set the configuration key named ``eggs`` to ``small``.

It is also possible to define a section multiple times. A section can
be redefined on the same and/or on different configuration files. For
example::

    [foo]
    eggs=large
    ham=serrano
    eggs=small

    [bar]
    eggs=ham
    green=
       eggs

    [foo]
    ham=prosciutto
    eggs=medium
    bread=toasted

This would set the ``eggs``, ``ham``, and ``bread`` configuration keys
of the ``foo`` section to ``medium``, ``prosciutto``, and ``toasted``,
respectively. As you can see there only thing that matters is the last
value that was set for each of the configuration keys.

If a configuration key is set multiple times in different
configuration files the final value will depend on the order in which
the different configuration files are read, with settings from earlier
paths overriding later ones as described on the ``Files`` section
above.

A line of the form ``%include file`` will include ``file`` into the
current configuration file. The inclusion is recursive, which means
that included files can include other files. Filenames are relative to
the configuration file in which the ``%include`` directive is found.
Environment variables and ``~user`` constructs are expanded in
``file``. This lets you do something like::

  %include ~/.@prog@/$HOST.config

to include a different configuration file on each computer you use.

A line with ``%unset name`` will remove ``name`` from the current
section, if it has been set previously.

The values are either free-form text strings, lists of text strings,
or Boolean values. Boolean values can be set to true using any of "1",
"yes", "true", or "on" and to false using "0", "no", "false", or "off"
(all case insensitive).

List values are separated by whitespace or comma, except when values are
placed in double quotation marks::

  allow_read = "John Doe, PhD", brian, betty

Quotation marks can be escaped by prefixing them with a backslash. Only
quotation marks at the beginning of a word is counted as a quotation
(e.g., ``foo"bar baz`` is the list of ``foo"bar`` and ``baz``).

Sections
========

This section describes the different sections that may appear in a
@Product@ configuration file, the purpose of each section, its possible
keys, and their possible values.

``alias``
---------

Defines command aliases.

Aliases allow you to define your own commands in terms of other
commands (or aliases), optionally including arguments. Positional
arguments in the form of ``$1``, ``$2``, etc. in the alias definition
are expanded by @Product@ before execution. Positional arguments not
already used by ``$N`` in the definition are put at the end of the
command to be executed.

Alias definitions consist of lines of the form::

    <alias> = <command> [<argument>]...

For example, this definition::

    latest = log --limit 5

creates a new command ``latest`` that shows only the five most recent
changesets. You can define subsequent aliases using earlier ones::

    stable5 = latest -b stable

.. note::

   It is possible to create aliases with the same names as
   existing commands, which will then override the original
   definitions. This is almost always a bad idea!

An alias can start with an exclamation point (``!``) to make it a
shell alias. A shell alias is executed with the shell and will let you
run arbitrary commands. As an example, ::

   echo = !echo $@

will let you do ``@prog@ echo foo`` to have ``foo`` printed in your
terminal. A better example might be::

   purge = !$HG status --no-status --unknown -0 re: | xargs -0 rm -f

which will make ``@prog@ purge`` delete all unknown files in the
repository in the same manner as the purge extension.

Positional arguments like ``$1``, ``$2``, etc. in the alias definition
expand to the command arguments. Unmatched arguments are
removed. ``$0`` expands to the alias name and ``$@`` expands to all
arguments separated by a space. ``"$@"`` (with quotes) expands to all
arguments quoted individually and separated by a space. These expansions
happen before the command is passed to the shell.

Shell aliases are executed in an environment where ``$HG`` expands to
the path of the @Product@ that was used to execute the alias. This is
useful when you want to call further @Product@ commands in a shell
alias, as was done above for the purge alias. In addition,
``$HG_ARGS`` expands to the arguments given to @Product@. In the ``hg
echo foo`` call above, ``$HG_ARGS`` would expand to ``echo foo``.

.. note::

   Some global configuration options such as ``-R`` are
   processed before shell aliases and will thus not be passed to
   aliases.


``annotate``
------------

Settings used when displaying file annotations. All values are
Booleans and default to False. See :prog:`help config.diff` for
related options for the diff command.

``ignorews``
    Ignore white space when comparing lines.

``ignorewseol``
    Ignore white space at the end of a line when comparing lines.

``ignorewsamount``
    Ignore changes in the amount of white space.

``ignoreblanklines``
    Ignore changes whose lines are all blank.


``auth``
--------

Authentication credentials and other authentication-like configuration
for HTTP connections. This section allows you to store usernames and
passwords for use when logging *into* HTTP servers.

The following options apply to all hosts.

``cookiefile``
    Path to a file containing HTTP cookie lines. Cookies matching a
    host will be sent automatically.

    The file format uses the Mozilla cookies.txt format, which defines cookies
    on their own lines. Each line contains 7 fields delimited by the tab
    character (domain, is_domain_cookie, path, is_secure, expires, name,
    value). For more info, do an Internet search for "Netscape cookies.txt
    format."

    Note: the cookies parser does not handle port numbers on domains. You
    will need to remove ports from the domain for the cookie to be recognized.
    This could result in a cookie being disclosed to an unwanted server.

    The cookies file is read-only.

Other options in this section are grouped by name and have the following
format::

    <name>.<argument> = <value>

where ``<name>`` is used to group arguments into authentication
entries. Example::

    foo.prefix = hg.intevation.de/mercurial
    foo.username = foo
    foo.password = bar
    foo.schemes = http https

    bar.prefix = secure.example.org
    bar.key = path/to/file.key
    bar.cert = path/to/file.cert
    bar.schemes = https

Supported arguments:

``prefix``
    Either ``*`` or a URI prefix with or without the scheme part.
    The authentication entry with the longest matching prefix is used
    (where ``*`` matches everything and counts as a match of length
    1). If the prefix doesn't include a scheme, the match is performed
    against the URI with its scheme stripped as well, and the schemes
    argument, q.v., is then subsequently consulted.

``username``
    Optional. Username to authenticate with. If not given, and the
    remote site requires basic or digest authentication, the user will
    be prompted for it. Environment variables are expanded in the
    username letting you do ``foo.username = $USER``. If the URI
    includes a username, only ``[auth]`` entries with a matching
    username or without a username will be considered.

``password``
    Optional. Password to authenticate with. If not given, and the
    remote site requires basic or digest authentication, the user
    will be prompted for it.

``key``
    Optional. PEM encoded client certificate key file. Environment
    variables are expanded in the filename.

``cert``
    Optional. PEM encoded client certificate chain file. Environment
    variables are expanded in the filename.

``schemes``
    Optional. Space separated list of URI schemes to use this
    authentication entry with. Only used if the prefix doesn't include
    a scheme. Supported schemes are http and https. They will match
    static-http and static-https respectively, as well.
    (default: https)

If no suitable authentication entry is found, the user is prompted
for credentials as usual if required by the remote.

``color``
---------

Configure the @Product@ color mode. For details about how to define your custom
effect and style see :prog:`help color`.

``mode``
    String: control the method used to output color. One of ``auto``, ``ansi``,
    ``win32``, or ``debug``. In auto mode, @Product@ will use ANSI mode by
    default (or win32 mode prior to Windows 10) if it detects a terminal. Any
    invalid value will disable color.

``commands``
------------

``status.relative``
    Make paths in :prog:`status` output relative to the current directory.
    (default: False)

``update.check``
    Determines what level of checking :prog:`goto` will perform before moving to
    a destination revision. Valid values are ``abort``, ``none``, and
    ``noconflict``. ``abort`` always fails if the working directory has
    uncommitted changes. ``none`` performs no checking, and may result in a
    merge with uncommitted changes. ``noconflict`` will allow any update which
    would not trigger a merge with uncommitted changes, if any are present.
    (default: ``noconflict``)

``commit``
----------

``description-size-limit``
    Maximum length (in bytes) of the description for new commits.  Set this
    to a reasonable value to prevent accidentally creating commits with
    very large commit messages.

``extras-size-limit``
    Maximum total size (in bytes) of the commit extras for new commits.
    Set this to a reasonable value to prevent accidentally creating commits
    with very large metadata.

``committemplate``
------------------

``changeset``
    String: configuration in this section is used as the template to
    customize the text shown in the editor when committing.

In addition to pre-defined template keywords, commit log specific one
below can be used for customization:

For example, the template configuration below shows as same text as
one shown by default::

    [committemplate]
    changeset = {desc}\n\n
        @PROG@: Enter commit message.  Lines beginning with '@PROG@:' are removed.
        @PROG@: Leave message empty to abort commit.
        @PROG@: --
        @PROG@: user: {author}\n{ifeq(p2rev, "-1", "",
       "@PROG@: branch merge\n")
       }@PROG@: branch '{branch}'\n{if(activebookmark,
       "@PROG@: bookmark '{activebookmark}'\n")   }{file_adds %
       "@PROG@: added {file}\n"                   }{file_mods %
       "@PROG@: changed {file}\n"                 }{file_dels %
       "@PROG@: removed {file}\n"                 }{if(files, "",
       "@PROG@: no files changed\n")}

``diff()``
    String: show the diff (see :prog:`help templates` for detail)

Sometimes it is helpful to show the diff of the changeset in the editor without
having to prefix '@PROG@: ' to each line so that highlighting works correctly. For
this, @Product@ provides a special string which will ignore everything below
it::

     @PROG@: ------------------------ >8 ------------------------

For example, the template configuration below will show the diff below the
extra message::

    [committemplate]
    changeset = {desc}\n\n
        @PROG@: Enter commit message.  Lines beginning with '@PROG@:' are removed.
        @PROG@: Leave message empty to abort commit.
        @PROG@: ------------------------ >8 ------------------------
        @PROG@: Do not touch the line above.
        @PROG@: Everything below will be removed.
        {diff()}

.. note::

   For some problematic encodings (see :prog:`help win32mbcs` for
   detail), this customization should be configured carefully, to
   avoid showing broken characters.

   For example, if a multibyte character ending with backslash (0x5c) is
   followed by the ASCII character 'n' in the customized template,
   the sequence of backslash and 'n' is treated as line-feed unexpectedly
   (and the multibyte character is broken, too).

Customized template is used for commands below (``--edit`` may be
required):

- :prog:`backout`
- :prog:`commit`
- :prog:`fetch` (for merge commit only)
- :prog:`graft`
- :prog:`histedit`
- :prog:`import`
- :prog:`qfold`, :prog:`qnew` and :prog:`qrefresh`
- :prog:`rebase`
- :prog:`shelve`
- :prog:`sign`
- :prog:`tag`
- :prog:`transplant`

Configuring items below instead of ``changeset`` allows showing
customized message only for specific actions, or showing different
messages for each action.

- ``changeset.backout`` for :prog:`backout`
- ``changeset.commit.amend.merge`` for :prog:`commit --amend` on merges
- ``changeset.commit.amend.normal`` for :prog:`commit --amend` on other
- ``changeset.commit.normal.merge`` for :prog:`commit` on merges
- ``changeset.commit.normal.normal`` for :prog:`commit` on other
- ``changeset.fetch`` for :prog:`fetch` (impling merge commit)
- ``changeset.gpg.sign`` for :prog:`sign`
- ``changeset.graft`` for :prog:`graft`
- ``changeset.histedit.edit`` for ``edit`` of :prog:`histedit`
- ``changeset.histedit.fold`` for ``fold`` of :prog:`histedit`
- ``changeset.histedit.mess`` for ``mess`` of :prog:`histedit`
- ``changeset.histedit.pick`` for ``pick`` of :prog:`histedit`
- ``changeset.import.bypass`` for :prog:`import --bypass`
- ``changeset.import.normal.merge`` for :prog:`import` on merges
- ``changeset.import.normal.normal`` for :prog:`import` on other
- ``changeset.rebase.collapse`` for :prog:`rebase --collapse`
- ``changeset.rebase.merge`` for :prog:`rebase` on merges
- ``changeset.rebase.normal`` for :prog:`rebase` on other
- ``changeset.shelve.shelve`` for :prog:`shelve`
- ``changeset.tag.add`` for :prog:`tag` without ``--remove``
- ``changeset.tag.remove`` for :prog:`tag --remove`
- ``changeset.transplant.merge`` for :prog:`transplant` on merges
- ``changeset.transplant.normal`` for :prog:`transplant` on other

These dot-separated lists of names are treated as hierarchical ones.
For example, ``changeset.tag.remove`` customizes the commit message
only for :prog:`tag --remove`, but ``changeset.tag`` customizes the
commit message for :prog:`tag` regardless of ``--remove`` option.

When the external editor is invoked for a commit, the corresponding
dot-separated list of names without the ``changeset.`` prefix
(e.g. ``commit.normal.normal``) is in the ``HGEDITFORM`` environment
variable.

In this section, items other than ``changeset`` can be referred from
others. For example, the configuration to list committed files up
below can be referred as ``{listupfiles}``::

    [committemplate]
    listupfiles = {file_adds %
       "@PROG@: added {file}\n"     }{file_mods %
       "@PROG@: changed {file}\n"   }{file_dels %
       "@PROG@: removed {file}\n"   }{if(files, "",
       "@PROG@: no files changed\n")}

``common``
------------------

``reponame``
    String: Name of the repo. Mostly intended to be used server-side to get
    the canonical name of the repository

``connectionpool``
------------------

``lifetime``
    Number of seconds for which connections in the connection pool can be kept
    and reused.  Connections that are older than this won't be reused.


``defaults``
------------

(defaults are deprecated. Don't use them. Use aliases instead.)

Use the ``[defaults]`` section to define command defaults, i.e. the
default options/arguments to pass to the specified commands.

The following example makes :prog:`log` run in verbose mode, and
:prog:`status` show only the modified files, by default::

  [defaults]
  log = -v
  status = -m

The actual commands, instead of their aliases, must be used when
defining command defaults. The command defaults will also be applied
to the aliases of the commands defined.


``diff``
--------

Settings used when displaying diffs. Everything except for ``unified``
is a Boolean and defaults to False. See :prog:`help config.annotate`
for related options for the annotate command.

``git``
    Use git extended diff format.

``nobinary``
    Omit git binary patches.

``nodates``
    Don't include dates in diff headers.

``noprefix``
    Omit 'a/' and 'b/' prefixes from filenames. Ignored in plain mode.

``showfunc``
    Show which function each change is in.

``ignorews``
    Ignore white space when comparing lines.

``ignorewsamount``
    Ignore changes in the amount of white space.

``ignoreblanklines``
    Ignore changes whose lines are all blank.

``unified``
    Number of lines of context to show.

``hashbinary``
    Show a SHA-1 hash of changed binaries in diff output.

``filtercopysource``
    Ignore copies or renames if the source path is outside file patterns.


``edenfs``
---------

Options that control the behavior of EdenFS.

``tree-fetch-depth``
    How many layers of children trees to fetch when downloading a directory listing from
    the servers.  Higher values increase the latency of individual fetch operations, but
    potentially help save having to send separate fetch requests later to download any
    child trees that are needed.


``email``
---------

Settings for extensions that send email messages.

``from``
    Optional. Email address to use in "From" header and SMTP envelope
    of outgoing messages.

``to``
    Optional. Comma-separated list of recipients' email addresses.

``cc``
    Optional. Comma-separated list of carbon copy recipients'
    email addresses.

``bcc``
    Optional. Comma-separated list of blind carbon copy recipients'
    email addresses.

``method``
    Optional. Method to use to send email messages. If value is ``smtp``
    (default), use SMTP (see the ``[smtp]`` section for configuration).
    Otherwise, use as name of program to run that acts like sendmail
    (takes ``-f`` option for sender, list of recipients on command line,
    message on stdin). Normally, setting this to ``sendmail`` or
    ``/usr/sbin/sendmail`` is enough to use sendmail to send messages.

``charsets``
    Optional. Comma-separated list of character sets considered
    convenient for recipients. Addresses, headers, and parts not
    containing patches of outgoing messages will be encoded in the
    first character set to which conversion from local encoding
    (``$HGENCODING``, ``ui.fallbackencoding``) succeeds. If correct
    conversion fails, the text in question is sent as is.
    (default: '')

    Order of outgoing email character sets:

    1. ``us-ascii``: always first, regardless of settings
    2. ``email.charsets``: in order given by user
    3. ``ui.fallbackencoding``: if not in email.charsets
    4. ``$HGENCODING``: if not in email.charsets
    5. ``utf-8``: always last, regardless of settings

Email example::

  [email]
  from = Joseph User <joe.user@example.com>
  method = /usr/sbin/sendmail
  # charsets for western Europeans
  # us-ascii, utf-8 omitted, as they are tried first and last
  charsets = iso-8859-1, iso-8859-15, windows-1252


``experimental``
----------------

``experimental.graph.show-abbreviated-ancestors``
    When showing a commit graph, show or hide ancestor commits outside the
    selected revset using a marker like '~'.

    If set to "always" or True (default), then '~' is always shown if an
    ancestor commit exists. For example:

      o    merge commit
      ├─╮
      │ │
      │ ~
      │
      o  commit
      │
      ~

    If set to "onlymerge", then '~' is shown only there is a visible sibling
    commit due to a merge. For example:

      o    merge commit
      ├─╮
      │ │
      │ ~
      │
      o  commit

    If set to "never" or False, then '~' is never shown. For example:

      o  merge commit
      │
      o  commit

    To change the look of the '~' indicator, see the
    ``experimental.graph.renderer`` setting.


``extensions``
--------------

@Product@ has an extension mechanism for adding new features. To
enable an extension, create an entry for it in this section.

If you know that the extension is already in Python's search path,
you can give the name of the module, followed by ``=``, with nothing
after the ``=``.

Otherwise, give a name that you choose, followed by ``=``, followed by
the path to the ``.py`` file (including the file name extension) that
defines the extension.

To explicitly disable an extension that is enabled in a config file of
broader scope, prepend its path with ``!``, as in ``foo = !/ext/path``
or ``foo = !`` when path is not supplied.

Example for ``sapling.conf``::

  [extensions]
  # (the amend extension will get loaded from @Product@'s path)
  amend =
  # (this extension will get loaded from the file specified)
  myfeature = ~/.ext/myfeature.py


``format``
----------

``usegeneraldelta``
    Enable or disable the "generaldelta" repository format which improves
    repository compression by allowing "revlog" to store delta against arbitrary
    revision instead of the previous stored one. This provides significant
    improvement for repositories with branches.

    Repositories with this on-disk format require @Product@ version 1.9.

    Enabled by default.

``dirstate``
    Dirstate format version to use. One of 0 (flat dirstate), and 2 (treestate).
    Default is 2.

``uselz4``
    Enable or disable the lz4 compression format on the revlogs.

``cgdeltabase``
    Control the delta base of revisions in a changegroup. Could be one of:
    "default", "no-external", "always-null", or "default". "default" means
    delta base can be any revision. "no-external" limits delta bases to be
    only revisions in a same changegroup. "always-null" enforces deltas to be
    the "null" revision, effectively making revisions full texts.

    Default: "default".

``graph``
---------

Web graph view configuration. This section let you change graph
elements display properties by branches, for instance to make the
``default`` branch stand out.

Each line has the following format::

    <branch>.<argument> = <value>

where ``<branch>`` is the name of the branch being
customized. Example::

    [graph]
    # 2px width
    default.width = 2
    # red color
    default.color = FF0000

Supported arguments:

``width``
    Set branch edges width in pixels.

``color``
    Set branch edges color in hexadecimal RGB notation.

``help``
--------

``localhelp``
    Additional information to display at the end of ``@prog@ help``.

``hint``
--------

Some commands show hints about features, like::

    hint[import]: use '@prog@ import' to import commits exported by '@prog@ export'

They can be silenced by ``@prog@ hint --ack import``, which writes the
``hint.ack`` config to the user config.

``ack``
    A list of hint IDs that were acknowledged so they will not
    be shown again. If set to ``*``, silence all hints.

``hooks``
---------

Commands or Python functions that get automatically executed by
various actions such as starting or finishing a commit. Multiple
hooks can be run for the same action by appending a suffix to the
action. Overriding a site-wide hook can be done by changing its
value or setting it to an empty string.  Hooks can be prioritized
by adding a prefix of ``priority.`` to the hook name on a new line
and setting the priority. The default priority is 0.

Example ``.@prog@/config``::

  [hooks]
  # update working directory after adding changesets
  changegroup.update = @prog@ goto
  # do not use the site-wide hook
  incoming =
  incoming.email = /my/email/hook
  incoming.autobuild = /my/build/hook
  # force autobuild hook to run before other incoming hooks
  priority.incoming.autobuild = 1

Most hooks are run with environment variables set that give useful
additional information. For each hook below, the environment variables
it is passed are listed with names in the form ``$HG_foo``. The
``$HG_HOOKTYPE`` and ``$HG_HOOKNAME`` variables are set for all hooks.
They contain the type of hook which triggered the run and the full name
of the hook in the config, respectively. In the example above, this will
be ``$HG_HOOKTYPE=incoming`` and ``$HG_HOOKNAME=incoming.email``.

``changegroup``
  Run after a changegroup has been added via push, pull or unbundle.  The ID of
  the first new changeset is in ``$HG_NODE`` and last is in ``$HG_NODE_LAST``.
  The URL from which changes came is in ``$HG_URL``.

``commit``
  Run after a changeset has been created in the local repository. The ID
  of the newly created changeset is in ``$HG_NODE``. Parent changeset
  IDs are in ``$HG_PARENT1`` and ``$HG_PARENT2``.

``incoming``
  Run after a changeset has been pulled, pushed, or unbundled into
  the local repository. The ID of the newly arrived changeset is in
  ``$HG_NODE``. The URL that was source of the changes is in ``$HG_URL``.

``outgoing``
  Run after sending changes from the local repository to another. The ID of
  first changeset sent is in ``$HG_NODE``. The source of operation is in
  ``$HG_SOURCE``. Also see :prog:`help config.hooks.preoutgoing`.

``post-<command>``
  Run after successful invocations of the associated command. The
  contents of the command line are passed as ``$HG_ARGS`` and the result
  code in ``$HG_RESULT``. Parsed command line arguments are passed as
  ``$HG_PATS`` and ``$HG_OPTS``. These contain string representations of
  the python data internally passed to <command>. ``$HG_OPTS`` is a
  dictionary of options (with unspecified options set to their defaults).
  ``$HG_PATS`` is a list of arguments. Hook failure is ignored.

``fail-<command>``
  Run after a failed invocation of an associated command. The contents
  of the command line are passed as ``$HG_ARGS``. Parsed command line
  arguments are passed as ``$HG_PATS`` and ``$HG_OPTS``. These contain
  string representations of the python data internally passed to
  <command>. ``$HG_OPTS`` is a dictionary of options (with unspecified
  options set to their defaults). ``$HG_PATS`` is a list of arguments.
  Hook failure is ignored.

``pre-<command>``
  Run before executing the associated command. The contents of the
  command line are passed as ``$HG_ARGS``. Parsed command line arguments
  are passed as ``$HG_PATS`` and ``$HG_OPTS``. These contain string
  representations of the data internally passed to <command>. ``$HG_OPTS``
  is a dictionary of options (with unspecified options set to their
  defaults). ``$HG_PATS`` is a list of arguments. If the hook returns
  failure, the command doesn't execute and @Product@ returns the failure
  code.

``prechangegroup``
  Run before a changegroup is added via push, pull or unbundle. Exit
  status 0 allows the changegroup to proceed. A non-zero status will
  cause the push, pull or unbundle to fail. The URL from which changes
  will come is in ``$HG_URL``.

``precommit``
  Run before starting a local commit. Exit status 0 allows the
  commit to proceed. A non-zero status will cause the commit to fail.
  Parent changeset IDs are in ``$HG_PARENT1`` and ``$HG_PARENT2``.

``prelistkeys``
  Run before listing pushkeys (like bookmarks) in the
  repository. A non-zero status will cause failure. The key namespace is
  in ``$HG_NAMESPACE``.

``preoutgoing``
  Run before collecting changes to send from the local repository to
  another. A non-zero status will cause failure. This lets you prevent
  pull over HTTP or SSH. It can also prevent propagating commits (via
  local pull, push (outbound) or bundle commands), but not completely,
  since you can just copy files instead. The source of operation is in
  ``$HG_SOURCE``. If "serve", the operation is happening on behalf of a remote
  SSH or HTTP repository. If "push", "pull" or "bundle", the operation
  is happening on behalf of a repository on same system.

``prepushkey``
  Run before a pushkey (like a bookmark) is added to the
  repository. A non-zero status will cause the key to be rejected. The
  key namespace is in ``$HG_NAMESPACE``, the key is in ``$HG_KEY``,
  the old value (if any) is in ``$HG_OLD``, and the new value is in
  ``$HG_NEW``.

``pretag``
  Run before creating a tag. Exit status 0 allows the tag to be
  created. A non-zero status will cause the tag to fail. The ID of the
  changeset to tag is in ``$HG_NODE``. The name of tag is in ``$HG_TAG``. The
  tag is local if ``$HG_LOCAL=1``, or in the repository if ``$HG_LOCAL=0``.

``pretxnopen``
  Run before any new repository transaction is open. The reason for the
  transaction will be in ``$HG_TXNNAME``, and a unique identifier for the
  transaction will be in ``HG_TXNID``. A non-zero status will prevent the
  transaction from being opened.

``pretxnclose``
  Run right before the transaction is actually finalized. Any repository change
  will be visible to the hook program. This lets you validate the transaction
  content or change it. Exit status 0 allows the commit to proceed. A non-zero
  status will cause the transaction to be rolled back. The reason for the
  transaction opening will be in ``$HG_TXNNAME``, and a unique identifier for
  the transaction will be in ``HG_TXNID``. The rest of the available data will
  vary according the transaction type. New changesets will add ``$HG_NODE``
  (the ID of the first added changeset), ``$HG_NODE_LAST`` (the ID of the last
  added changeset), ``$HG_URL`` and ``$HG_SOURCE`` variables.  Bookmark and
  phase changes will set ``HG_BOOKMARK_MOVED`` and ``HG_PHASES_MOVED`` to ``1``
  respectively, etc.

``pretxnclose-bookmark``
  Run right before a bookmark change is actually finalized. Any repository
  change will be visible to the hook program. This lets you validate the
  transaction content or change it. Exit status 0 allows the commit to
  proceed. A non-zero status will cause the transaction to be rolled back.
  The name of the bookmark will be available in ``$HG_BOOKMARK``, the new
  bookmark location will be available in ``$HG_NODE`` while the previous
  location will be available in ``$HG_OLDNODE``. In case of a bookmark
  creation ``$HG_OLDNODE`` will be empty. In case of deletion ``$HG_NODE``
  will be empty.
  In addition, the reason for the transaction opening will be in
  ``$HG_TXNNAME``, and a unique identifier for the transaction will be in
  ``HG_TXNID``.

``pretxnclose-phase``
  Run right before a phase change is actually finalized. Any repository change
  will be visible to the hook program. This lets you validate the transaction
  content or change it. Exit status 0 allows the commit to proceed.  A non-zero
  status will cause the transaction to be rolled back. The hook is called
  multiple times, once for each revision affected by a phase change.
  The affected node is available in ``$HG_NODE``, the phase in ``$HG_PHASE``
  while the previous ``$HG_OLDPHASE``. In case of new node, ``$HG_OLDPHASE``
  will be empty.  In addition, the reason for the transaction opening will be in
  ``$HG_TXNNAME``, and a unique identifier for the transaction will be in
  ``HG_TXNID``. The hook is also run for newly added revisions. In this case
  the ``$HG_OLDPHASE`` entry will be empty.

``txnclose``
  Run after any repository transaction has been committed. At this
  point, the transaction can no longer be rolled back. The hook will run
  after the lock is released. See :prog:`help config.hooks.pretxnclose` for
  details about available variables.

``txnclose-bookmark``
  Run after any bookmark change has been committed. At this point, the
  transaction can no longer be rolled back. The hook will run after the lock
  is released. See :prog:`help config.hooks.pretxnclose-bookmark` for details
  about available variables.

``txnclose-phase``
  Run after any phase change has been committed. At this point, the
  transaction can no longer be rolled back. The hook will run after the lock
  is released. See :prog:`help config.hooks.pretxnclose-phase` for details about
  available variables.

``txnabort``
  Run when a transaction is aborted. See :prog:`help config.hooks.pretxnclose`
  for details about available variables.

``pretxnchangegroup``
  Run after a changegroup has been added via push, pull or unbundle, but before
  the transaction has been committed. The changegroup is visible to the hook
  program. This allows validation of incoming changes before accepting them.
  The ID of the first new changeset is in ``$HG_NODE`` and last is in
  ``$HG_NODE_LAST``. Exit status 0 allows the transaction to commit. A non-zero
  status will cause the transaction to be rolled back, and the push, pull or
  unbundle will fail. The URL that was the source of changes is in ``$HG_URL``.

``pretxncommit``
  Run after a changeset has been created, but before the transaction is
  committed. The changeset is visible to the hook program. This allows
  validation of the commit message and changes. Exit status 0 allows the
  commit to proceed. A non-zero status will cause the transaction to
  be rolled back. The ID of the new changeset is in ``$HG_NODE``. The parent
  changeset IDs are in ``$HG_PARENT1`` and ``$HG_PARENT2``.

``preupdate``
  Run before updating the working directory. Exit status 0 allows
  the update to proceed. A non-zero status will prevent the update.
  The changeset ID of first new parent is in ``$HG_PARENT1``. If updating to a
  merge, the ID of second new parent is in ``$HG_PARENT2``.

``listkeys``
  Run after listing pushkeys (like bookmarks) in the repository. The
  key namespace is in ``$HG_NAMESPACE``. ``$HG_VALUES`` is a
  dictionary containing the keys and values.

``pushkey``
  Run after a pushkey (like a bookmark) is added to the
  repository. The key namespace is in ``$HG_NAMESPACE``, the key is in
  ``$HG_KEY``, the old value (if any) is in ``$HG_OLD``, and the new
  value is in ``$HG_NEW``.

``tag``
  Run after a tag is created. The ID of the tagged changeset is in ``$HG_NODE``.
  The name of tag is in ``$HG_TAG``. The tag is local if ``$HG_LOCAL=1``, or in
  the repository if ``$HG_LOCAL=0``.

``update``
  Run after updating the working directory. The changeset ID of first
  new parent is in ``$HG_PARENT1``. If updating to a merge, the ID of second new
  parent is in ``$HG_PARENT2``. If the update succeeded, ``$HG_ERROR=0``. If the
  update failed (e.g. because conflicts were not resolved), ``$HG_ERROR=1``.

.. note::

   It is generally better to use standard hooks rather than the
   generic pre- and post- command hooks, as they are guaranteed to be
   called in the appropriate contexts for influencing transactions.
   Also, hooks like "commit" will be called in all contexts that
   generate a commit (e.g. tag) and not just the commit command.

.. note::

   Environment variables with empty values may not be passed to
   hooks on platforms such as Windows. As an example, ``$HG_PARENT2``
   will have an empty value under Unix-like platforms for non-merge
   changesets, while it will not be available at all under Windows.

.. note::

    A hook can start with ``background:``. It will be spawned in the
    background and its result will be discarded.

The syntax for Python hooks is as follows::

  hookname = python:modulename.submodule.callable
  hookname = python:/path/to/python/module.py:callable

Python hooks are run within the @Product@ process. Each hook is
called with at least three keyword arguments: a ui object (keyword
``ui``), a repository object (keyword ``repo``), and a ``hooktype``
keyword that tells what kind of hook is used. Arguments listed as
environment variables above are passed as keyword arguments, with no
``HG_`` prefix, and names in lower case.

If a Python hook returns a "true" value or raises an exception, this
is treated as a failure.


``hostfingerprints``
--------------------

(Deprecated. Use ``[hostsecurity]``'s ``fingerprints`` options instead.)

Fingerprints of the certificates of known HTTPS servers.

A HTTPS connection to a server with a fingerprint configured here will
only succeed if the servers certificate matches the fingerprint.
This is very similar to how ssh known hosts works.

The fingerprint is the SHA-1 hash value of the DER encoded certificate.
Multiple values can be specified (separated by spaces or commas). This can
be used to define both old and new fingerprints while a host transitions
to a new certificate.

The CA chain and web.cacerts is not used for servers with a fingerprint.

For example::

    [hostfingerprints]
    hg.intevation.de = fc:e2:8d:d9:51:cd:cb:c1:4d:18:6b:b7:44:8d:49:72:57:e6:cd:33
    hg.intevation.org = fc:e2:8d:d9:51:cd:cb:c1:4d:18:6b:b7:44:8d:49:72:57:e6:cd:33

``hostsecurity``
----------------

Used to specify global and per-host security settings for connecting to
other machines.

The following options control default behavior for all hosts.

``ciphers``
    Defines the cryptographic ciphers to use for connections.

    Value must be a valid OpenSSL Cipher List Format as documented at
    https://www.openssl.org/docs/manmaster/apps/ciphers.html#CIPHER-LIST-FORMAT.

    This setting is for advanced users only. Setting to incorrect values
    can significantly lower connection security or decrease performance.
    You have been warned.

    This option requires Python 2.7.

``minimumprotocol``
    Defines the minimum channel encryption protocol to use.

    By default, the highest version of TLS supported by both client and server
    is used.

    Allowed values are: ``tls1.0``, ``tls1.1``, ``tls1.2``.

    When running on an old Python version, only ``tls1.0`` is allowed since
    old versions of Python only support up to TLS 1.0.

    When running a Python that supports modern TLS versions, the default is
    ``tls1.1``. ``tls1.0`` can still be used to allow TLS 1.0. However, this
    weakens security and should only be used as a feature of last resort if
    a server does not support TLS 1.1+.

Options in the ``[hostsecurity]`` section can have the form
``hostname``:``setting``. This allows multiple settings to be defined on a
per-host basis.

The following per-host settings can be defined.

``ciphers``
    This behaves like ``ciphers`` as described above except it only applies
    to the host on which it is defined.

``fingerprints``
    A list of hashes of the DER encoded peer/remote certificate. Values have
    the form ``algorithm``:``fingerprint``. e.g.
    ``sha256:c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2``.
    In addition, colons (``:``) can appear in the fingerprint part.

    The following algorithms/prefixes are supported: ``sha1``, ``sha256``,
    ``sha512``.

    Use of ``sha256`` or ``sha512`` is preferred.

    If a fingerprint is specified, the CA chain is not validated for this
    host and @Product@ will require the remote certificate to match one
    of the fingerprints specified. This means if the server updates its
    certificate, @Product@ will abort until a new fingerprint is defined.
    This can provide stronger security than traditional CA-based validation
    at the expense of convenience.

    This option takes precedence over ``verifycertsfile``.

``minimumprotocol``
    This behaves like ``minimumprotocol`` as described above except it
    only applies to the host on which it is defined.

``verifycertsfile``
    Path to file a containing a list of PEM encoded certificates used to
    verify the server certificate. Environment variables and ``~user``
    constructs are expanded in the filename.

    The server certificate or the certificate's certificate authority (CA)
    must match a certificate from this file or certificate verification
    will fail and connections to the server will be refused.

    If defined, only certificates provided by this file will be used:
    ``web.cacerts`` and any system/default certificates will not be
    used.

    This option has no effect if the per-host ``fingerprints`` option
    is set.

    The format of the file is as follows::

        -----BEGIN CERTIFICATE-----
        ... (certificate in base64 PEM encoding) ...
        -----END CERTIFICATE-----
        -----BEGIN CERTIFICATE-----
        ... (certificate in base64 PEM encoding) ...
        -----END CERTIFICATE-----

For example::

    [hostsecurity]
    hg.example.com:fingerprints = sha256:c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2
    hg2.example.com:fingerprints = sha1:914f1aff87249c09b6859b88b1906d30756491ca, sha1:fc:e2:8d:d9:51:cd:cb:c1:4d:18:6b:b7:44:8d:49:72:57:e6:cd:33
    hg3.example.com:fingerprints = sha256:9a:b0:dc:e2:75:ad:8a:b7:84:58:e5:1f:07:32:f1:87:e6:bd:24:22:af:b7:ce:8e:9c:b4:10:cf:b9:f4:0e:d2
    foo.example.com:verifycertsfile = /etc/ssl/trusted-ca-certs.pem

To change the default minimum protocol version to TLS 1.2 but to allow TLS 1.1
when connecting to ``hg.example.com``::

    [hostsecurity]
    minimumprotocol = tls1.2
    hg.example.com:minimumprotocol = tls1.1

``http_proxy``
--------------

Used to access web-based @Product@ repositories through a HTTP
proxy.

``host``
    Host name and (optional) port of the proxy server, for example
    "myproxy:8000".

``no``
    Optional. Comma-separated list of host names that should bypass
    the proxy.

``passwd``
    Optional. Password to authenticate with at the proxy server.

``user``
    Optional. User name to authenticate with at the proxy server.

``always``
    Optional. Always use the proxy, even for localhost and any entries
    in ``http_proxy.no``. (default: False)


``log``
-------

``simplify-grandparents``
   Controls whether grandparents of indeterminate commits get simplified
   for graph log. If set to true, edges from an indeterminate X to its
   (grand)parent Y will be hidden if X has another (grand)parent Z and
   Y is a (grand)parent of Z.

   For example::

      E
      |
      D
      |\
      B C
      |/
      A

   When logging A, B, E with this config off, the result is::

      E
      :\
      : B
      :/
      A

   When logging A, B, E with this config on, the result will be::

      E
      :
      B
      |
      A

   This feature requires a modern commit graph backend. It is turned
   off if an older commit graph backend is used. (default: True)


``merge``
---------

This section specifies behavior during merges and updates.

``checkignored``
   Controls behavior when an ignored file on disk has the same name as a tracked
   file in the changeset being merged or updated to, and has different
   contents. Options are ``abort``, ``warn`` and ``ignore``. With ``abort``,
   abort on such files. With ``warn``, warn on such files and back them up as
   ``.orig``. With ``ignore``, don't print a warning and back them up as
   ``.orig``. (default: ``abort``)

``checkunknown``
   Controls behavior when an unknown file that isn't ignored has the same name
   as a tracked file in the changeset being merged or updated to, and has
   different contents. Similar to ``merge.checkignored``, except for files that
   are not ignored. (default: ``abort``)

``on-failure``
   When set to ``continue`` (the default), the merge process attempts to
   merge all unresolved files using the merge chosen tool, regardless of
   whether previous file merge attempts during the process succeeded or not.
   Setting this to ``prompt`` will prompt after any merge failure continue
   or halt the merge process. Setting this to ``halt`` will automatically
   halt the merge process on any merge tool failure. The merge process
   can be restarted by using the ``resolve`` command. When a merge is
   halted, the repository is left in a normal ``unresolved`` merge state.
   (default: ``continue``)

``printcandidatecommmits``
   If set to ``true``, calculate and print potentially conflicted commits
   when there are merge conflicts. (default: ``false``)

``word-merge``
  When enabled, attempt to run word-level 3-way merge resolution on conflicted
  regions to reduce conflicts. This be helpful for some conflicts caused
  by renaming or small refactoring. (default: ``false``)

``merge-patterns``
------------------

This section specifies merge tools to associate with particular file
patterns. Tools matched here will take precedence over the default
merge tool. Patterns are globs by default, rooted at the repository
root.

Example::

  [merge-patterns]
  **.c = kdiff3
  **.jpg = myimgmerge

``merge-tools``
---------------

This section configures external merge tools to use for file-level
merges. This section has likely been preconfigured at install time.
Use :prog:`config merge-tools` to check the existing configuration.
Also see :prog:`help merge-tools` for more details.

Example ``sapling.conf``::

  [merge-tools]
  # Override stock tool location
  kdiff3.executable = ~/bin/kdiff3
  # Specify command line
  kdiff3.args = $base $local $other -o $output
  # Give higher priority
  kdiff3.priority = 1

  # Changing the priority of preconfigured tool
  meld.priority = 0

  # Disable a preconfigured tool
  vimdiff.disabled = yes

  # Define new tool
  myHtmlTool.args = -m $local $other $base $output
  myHtmlTool.regkey = Software\FooSoftware\HtmlMerge
  myHtmlTool.priority = 1

Supported arguments:

``priority``
  The priority in which to evaluate this tool.
  (default: 0)

``executable``
  Either just the name of the executable or its pathname.

  .. container:: windows

    On Windows, the path can use environment variables with ${ProgramFiles}
    syntax.

  (default: the tool name)

``args``
  The arguments to pass to the tool executable. You can refer to the
  files being merged as well as the output file through these
  variables: ``$base``, ``$local``, ``$other``, ``$output``. The meaning
  of ``$local`` and ``$other`` can vary depending on which action is being
  performed. During and update or merge, ``$local`` represents the original
  state of the file, while ``$other`` represents the commit you are updating
  to or the commit you are merging with. During a rebase ``$local``
  represents the destination of the rebase, and ``$other`` represents the
  commit being rebased.
  (default: ``$local $base $other``)

``premerge``
  Attempt to run internal non-interactive 3-way merge tool before
  launching external tool.  Options are ``true``, ``false``, ``keep``,
  ``keep-merge3`` or ``keep-mergediff``. The ``keep`` option will leave markers
  in the file if the premerge fails. The ``keep-merge3`` will do the same but
  include information about the base of the merge in the marker (see internal :merge3 in
  :prog:`help merge-tools`). The ``keep-mergediff`` option is similar but uses a
  a different marker style (see internal :mergediff in :hg:`help merge-tools`).
  (default: True)

``binary``
  This tool can merge binary files. (default: False, unless tool
  was selected by file pattern match)

``symlink``
  This tool can merge symlinks. (default: False)

``check``
  A list of merge success-checking options:

  ``changed``
    Ask whether merge was successful when the merged file shows no changes.
  ``conflicts``
    Check whether there are conflicts even though the tool reported success.
  ``prompt``
    Always prompt for merge success, regardless of success reported by tool.

``fixeol``
  Attempt to fix up EOL changes caused by the merge tool.
  (default: False)

``gui``
  This tool requires a graphical interface to run. (default: False)

.. container:: windows

  ``regkey``
    Windows registry key which describes install location of this
    tool. @Product@ will search for this key first under
    ``HKEY_CURRENT_USER`` and then under ``HKEY_LOCAL_MACHINE``.
    (default: None)

  ``regkeyalt``
    An alternate Windows registry key to try if the first key is not
    found.  The alternate key uses the same ``regname`` and ``regappend``
    semantics of the primary key.  The most common use for this key
    is to search for 32bit applications on 64bit operating systems.
    (default: None)

  ``regname``
    Name of value to read from specified registry key.
    (default: the unnamed (default) value)

  ``regappend``
    String to append to the value read from the registry, typically
    the executable name of the tool.
    (default: None)

``mutation``
------------

Controls recording of commit mutation metadata.

``enabled``
    Set to true to enable the usage of commit mutation metadata in preference
    to obsolescence markers.

``record``
    Set to false to disable recording of commit mutation metadata in commit
    extras.
    (default: True)

``date``
    Override the date and time the commit was mutated at.  The default is the
    current date and time.

``user``
    Override the username of the user performing the mutation.  The default is
    the current user.

``automigrate``
    Set to true to automatically convert obsmarkers to mutation metadata during
    automigration at the start of pull.

``allow-divergence``
    Set to true to allow operations to deliberately create divergence by
    rewriting or rebasing onto obsolete commits. (default: False)

``pager``
---------

Setting used to control when to paginate and with what external tool. See
:prog:`help pager` for details.

``pager``
    If specified as ``internal:streampager`` (the default), the builtin
    streampager is used, which supports showing progress bars and stderr.

    Otherwise, the config specifies the external tool used as pager.

``ignore``
    List of commands to disable the pager for. Example::

      [pager]
      ignore = version, help, update

``stderr``
    Whether to redirect error messages to the pager.

    If set to false, @Product@ will continue to output error messages and
    progress bars to stderr while the pager is running.  Depending on the
    pager, this may overlay the pager display.

    Has no effect if streampager is used.

``separate-stderr``
    Whether to show stderr in a separate pane.

    If set to false, stderr will be mixed into stdout output.

    Only affects streampager. Default: false.

``encoding``
    Encoding to use to output to the pager.

    If set, overrides the output encoding when sending output to the pager.

      [pager]
      encoding = utf-8

    Has no effect if streampager is used.

``scroll-past-eof``
    Whether to scroll past the end of the output. Default: false.

    Only affects streampager.

``mouse``
    Whether the pager handles the mouse, so that the scroll wheel scrolls
    the output. Default: false.

    Turning this on tells the terminal to send mouse events to the pager
    instead of acting on them itself, which stops click-and-drag text
    selection from working. Most terminals still let you select text while
    holding shift.

    Only affects streampager.

``wrapping-mode``
    Choose the line wrapping boundary: none, word, line.

    - none: do not wrap lines
    - word: wrap at word boundary
    - line: wrap at any characters

    Default: line

    Only affects streampager.

``interface``
    Choose the default interface: fullscreen, direct, hybrid, delayed:2000ms.

    - fullscreen: enter full screen interface immediately.
    - direct: output directly to terminal without buffering.
    - hybrid: direct for short output, fullscreen for large (>1 page) output.
    - delayed: buffer wait for the given time before outputting completed short
      output directly to the terminal, or entering fullscreen mode for longer
      output.

    Default: hybrid

    Only affects streampager.

``patch``
---------

Settings used when applying patches, for instance through the 'import'
command or with @Product@ Queues extension.

``eol``
    When set to 'strict' patch content and patched files end of lines
    are preserved. When set to ``lf`` or ``crlf``, both files end of
    lines are ignored when patching and the result line endings are
    normalized to either LF (Unix) or CRLF (Windows). When set to
    ``auto``, end of lines are again ignored while patching but line
    endings in patched files are normalized to their original setting
    on a per-file basis. If target file does not exist or has no end
    of line, patch line endings are preserved.
    (default: strict)

``fuzz``
    The number of lines of 'fuzz' to allow when applying patches. This
    controls how much context the patcher is allowed to ignore when
    trying to apply a patch.
    (default: 2)

``paths``
---------

Assigns symbolic names and behavior to repositories.

Options are symbolic names defining the URL or directory that is the
location of the repository. Example::

    [paths]
    my_server = https://example.com/my_repo
    local_path = /home/me/repo

These symbolic names can be used from the command line. To pull
from ``my_server``: :prog:`pull my_server`. To push to ``local_path``:
:prog:`push local_path`.

Options containing colons (``:``) denote sub-options that can influence
behavior for that specific path. Example::

    [paths]
    my_server = https://example.com/my_path
    my_server:pushurl = ssh://example.com/my_path

The following sub-options can be defined:

``pushurl``
   The URL to use for push operations. If not defined, the location
   defined by the path's main entry is used.

``pushrev``
   A revset defining which revisions to push by default.

   When :prog:`push` is executed without a ``-r`` argument, the revset
   defined by this sub-option is evaluated to determine what to push.

   For example, a value of ``.`` will push the working directory's
   revision by default.

   Revsets specifying bookmarks will not result in the bookmark being
   pushed.

The following special named paths exist:

``default``
   The URL or directory to use when no source or remote is specified.

   :prog:`clone` will automatically define this path to the location the
   repository was cloned from.

``default-push``
   (deprecated) The URL or directory for the default :prog:`push` location.
   ``default:pushurl`` should be used instead.

``phases``
----------

Specifies default handling of phases. See :prog:`help phases` for more
information about working with phases.

``publish``
    Controls draft phase behavior when working as a server. When true,
    pushed changesets are set to public in both client and server and
    pulled or cloned changesets are set to public in the client.
    (default: True)

``new-commit``
    Phase of newly-created commits.
    (default: draft)


``profiling``
-------------

Specifies profiling configuration. There are a native sampling profiler
that samples both Python and Rust stacks, and a few Python profilers.

Following configs affect the native sampling profiler::

``enabled``
    Enable the native sampling profiler. It covers both Python and Rust
    logic. (default: false)

    This is equivalent to passing ``--profile`` on the command line.

``output``
    File path where profiling data or report should be saved. If the
    file exists, it is appended. (default: None, data is printed on
    stderr)

``interval``
    Sampling profiler frequency.
    (default: 10ms)

``always-on-enabled``
    If ``enabled`` is false, and ``always-on-enabled`` is true,
    still turn on the profiling (usually with low frequency) to
    collect insights about slow runs.

``always-on-interval``
    Sampling profiler frequency for the "always-on" mode.
    (default: 1s)

Following configs affect the Python profiler::

``enabled-python``
    Enable the Python profiler. It only covers Python logic.
    (default: false)

    If ``profiling.enabled-python`` is false, alternative sections can still
    enable profiling. Sections starting with ``profiling:`` are examined in
    alphabet order. The first one with ``enabled`` set to true will be used.

``type``
    The type of profiler to use.
    (default: stat)

    ``ls``
      Use Python's built-in instrumenting profiler. This profiler
      works on all platforms, but each line number it reports is the
      first line of a function. This restriction makes it difficult to
      identify the expensive parts of a non-trivial function.
    ``stat``
      Use a statistical profiler, statprof. This profiler is most
      useful for profiling commands that run for longer than about 0.1
      seconds.
    ``traceprof``
      Use a tracing profiler. The profiler is good at counting high
      frequent small function calls.

``format``
    Profiling format.  Specific to the ``ls`` instrumenting profiler.
    (default: text)

    ``text``
      Generate a profiling report. When saving to a file, it should be
      noted that only the report is saved, and the profiling data is
      not kept.
    ``kcachegrind``
      Format profiling data for kcachegrind use: when saving to a
      file, the generated file can directly be loaded into
      kcachegrind.

``statformat``
    Profiling format for the ``stat`` profiler.
    (default: hotpath)

    ``hotpath``
      Show a tree-based display containing the hot path of execution (where
      most time was spent).
    ``bymethod``
      Show a table of methods ordered by how frequently they are active.
    ``byline``
      Show a table of lines in files ordered by how frequently they are active.
    ``json``
      Render profiling data as JSON.

``frequency``
    Sampling frequency.  Specific to the ``stat`` sampling profiler.
    (default: 1000)

``output``
    File path where profiling data or report should be saved. If the
    file exists, it is replaced. (default: None, data is printed on
    stderr)

``sort``
    Sort field.  Specific to the ``ls`` instrumenting profiler.
    One of ``callcount``, ``reccallcount``, ``totaltime`` and
    ``inlinetime``.
    (default: inlinetime)

``limit``
    Number of lines to show. Specific to the ``ls`` instrumenting profiler.
    (default: 30)

``minelapsed``
    Minimum seconds required to output the profiling result.
    (default: 0)

``nested``
    Show at most this number of lines of drill-down info after each main entry.
    This can help explain the difference between Total and Inline.
    Specific to the ``ls`` instrumenting profiler.
    (default: 5)

``showmin``
    Minimum fraction of samples an entry must have for it to be displayed.
    Can be specified as a float between ``0.0`` and ``1.0`` or can have a
    ``%`` afterwards to allow values up to ``100``. e.g. ``5%``.

    Only used by the ``stat`` profiler.

    For the ``hotpath`` format, default is ``0.05``.
    For the ``chrome`` format, default is ``0.005``.

    The option is unused on other formats.

``showmax``
    Maximum fraction of samples an entry can have before it is ignored in
    display. Values format is the same as ``showmin``.

    Only used by the ``stat`` profiler.

    For the ``chrome`` format, default is ``0.999``.

    The option is unused on other formats.

``progress``
------------

@Product@ commands can draw progress bars that are as informative as
possible. Some progress bars only offer indeterminate information, while others
have a definite end point.

``delay``
    Number of seconds (float) before showing the progress bar. (default: 3)

``changedelay``
    Minimum delay before showing a new topic. When set to less than 3 * refresh,
    that value will be used instead. (default: 1)

``estimateinterval``
    Maximum sampling interval in seconds for speed and estimated time
    calculation. (default: 60)

``refresh``
    Time in seconds between refreshes of the progress bar. (default: 0.1)

``format``
    Format of the progress bar.

    Valid entries for the format field are ``topic``, ``bar``, ``number``,
    ``unit``, ``estimate``, ``speed``, and ``item``. ``item`` defaults to the
    last 20 characters of the item, but this can be changed by adding either
    ``-<num>`` which would take the last num characters, or ``+<num>`` for the
    first num characters.

    (default: topic bar number estimate)

``width``
    If set, the maximum width of the progress information (that is, min(width,
    term width) will be used).

``clear-complete``
    Clear the progress bar after it's done. (default: True)

``disable``
    If true, don't show a progress bar.

``assume-tty``
    If true, ALWAYS show a progress bar, unless disable is given.

``renderer``
    The name of the renderer to use to render the progress bar.

``debug``
    Enables debug mode for progress bars.  Progress output will be printed line
    by line for each item that is processed.

``pull``
--------

``automigrate``
    Perform potentially expensive automatic migration to new formats and
    configurations at the start of pull commands. (default: True)

``rebase``
----------

``evolution.allowdivergence``
    Deprecated alias for ``mutation.allow-divergence``; still honored.
    Default to False, when True allow creating divergence when performing
    rebase of obsolete changesets.

``revsetalias``
---------------

Alias definitions for revsets. See :prog:`help revsets` for details.

``server``
----------

Controls generic server settings.

``bookmarks-pushkey-compat``
    Trigger pushkey hook when being pushed bookmark updates. This config exist
    for compatibility purpose (default to True)

    If you use ``pushkey`` and ``pre-pushkey`` hooks to control bookmark
    movement we recommend you migrate them to ``txnclose-bookmark`` and
    ``pretxnclose-bookmark``.

``compressionengines``
    List of compression engines and their relative priority to advertise
    to clients.

    The order of compression engines determines their priority, the first
    having the highest priority. If a compression engine is not listed
    here, it won't be advertised to clients.

    If not set (the default), built-in defaults are used. Run
    :prog:`debuginstall` to list available compression engines and their
    default wire protocol priority.

    Older @Product@ clients only support zlib compression and this setting
    has no effect for legacy clients.

``disablefullbundle``
    When set, servers will refuse attempts to do pull-based clones.
    If this option is set, clone bundles are highly recommended. Partial
    clones will still be allowed.
    (default: False)

``validate``
    Whether to validate the completeness of pushed changesets by
    checking that all new file revisions specified in manifests are
    present. (default: False)

``maxhttpheaderlen``
    Instruct HTTP clients not to send request headers longer than this
    many bytes. (default: 1024)

``bundle1``
    Whether to allow clients to push and pull using the legacy bundle1
    exchange format. (default: True)

``bundle1gd``
    Like ``bundle1`` but only used if the repository is using the
    *generaldelta* storage format. (default: True)

``bundle1.push``
    Whether to allow clients to push using the legacy bundle1 exchange
    format. (default: True)

``bundle1gd.push``
    Like ``bundle1.push`` but only used if the repository is using the
    *generaldelta* storage format. (default: True)

``bundle1.pull``
    Whether to allow clients to pull using the legacy bundle1 exchange
    format. (default: True)

``bundle1gd.pull``
    Like ``bundle1.pull`` but only used if the repository is using the
    *generaldelta* storage format. (default: True)

    Large repositories using the *generaldelta* storage format should
    consider setting this option because converting *generaldelta*
    repositories to the exchange format required by the bundle1 data
    format can consume a lot of CPU.

``zliblevel``
    Integer between ``-1`` and ``9`` that controls the zlib compression level
    for wire protocol commands that send zlib compressed output (notably the
    commands that send repository history data).

    The default (``-1``) uses the default zlib compression level, which is
    likely equivalent to ``6``. ``0`` means no compression. ``9`` means
    maximum compression.

    Setting this option allows server operators to make trade-offs between
    bandwidth and CPU used. Lowering the compression lowers CPU utilization
    but sends more bytes to clients.

    This option only impacts the HTTP server.

``zstdlevel``
    Integer between ``1`` and ``22`` that controls the zstd compression level
    for wire protocol commands. ``1`` is the minimal amount of compression and
    ``22`` is the highest amount of compression.

    The default (``3``) should be significantly faster than zlib while likely
    delivering better compression ratios.

    This option only impacts the HTTP server.

    See also ``server.zliblevel``.


``smtp``
--------

Configuration for extensions that need to send email messages.

``host``
    Host name of mail server, e.g. "mail.example.com".

``port``
    Optional. Port to connect to on mail server. (default: 465 if
    ``tls`` is smtps; 25 otherwise)

``tls``
    Optional. Method to enable TLS when connecting to mail server: starttls,
    smtps or none. (default: none)

``username``
    Optional. User name for authenticating with the SMTP server.
    (default: None)

``password``
    Optional. Password for authenticating with the SMTP server. If not
    specified, interactive sessions will prompt the user for a
    password; non-interactive sessions will fail. (default: None)

``local_hostname``
    Optional. The hostname that the sender can use to identify
    itself to the MTA.


``templatealias``
-----------------

Alias definitions for templates. See :prog:`help templates` for details.

``templates``
-------------

Use the ``[templates]`` section to define template strings.
See :prog:`help templates` for details.

``tracing``
-------------

``stderr``
    Whether to print the trace to stderr if it meets the ``tracing.threshold``
    cutoff.
    (default: false).

``threshold``
    Integer. Minimum duration, in seconds, a command must run in order for the
    trace to be logged (usually to the blackbox).
    (default: 10)

``treestate``
-------------

``mingcage``
    Seconds. Only files older than that would be garbage collected.
    (default: 1209600, 2 weeks)

``minrepackthreshold``
    Bytes. Minimal size to trigger a repack.
    (default: 10M)

``repackfactor``
    Integer. Number of times treestate can grow without being repacked.
    Set to 0 to disable automatic repack.
    (default: 3)


``ui``
------

User interface controls.

``archivemeta``
    Whether to include the .hg_archival.txt file containing meta data
    (hashes for the repository base and for tip) in archives created
    by the :prog:`archive` command.
    (default: True)

``askusername``
    Whether to prompt for a username when committing. If True, and
    neither ``$HGUSER`` has been specified, then the user will
    be prompted to enter a username. If no username is entered, the
    default ``USER@HOST`` is used instead.
    (default: False)

``clonebundles``
    Whether the "clone bundles" feature is enabled.

    When enabled, :prog:`clone` may download and apply a server-advertised
    bundle file from a URL instead of using the normal exchange mechanism.

    This can likely result in faster and more reliable clones.

    (default: True)

``clonebundlefallback``
    Whether failure to apply an advertised "clone bundle" from a server
    should result in fallback to a regular clone.

    This is disabled by default because servers advertising "clone
    bundles" often do so to reduce server load. If advertised bundles
    start mass failing and clients automatically fall back to a regular
    clone, this would add significant and unexpected load to the server
    since the server is expecting clone operations to be offloaded to
    pre-generated bundles. Failing fast (the default behavior) ensures
    clients don't overwhelm the server when "clone bundle" application
    fails.

    (default: False)

``clonebundleprefers``
    Defines preferences for which "clone bundles" to use.

    Servers advertising "clone bundles" may advertise multiple available
    bundles. Each bundle may have different attributes, such as the bundle
    type and compression format. This option is used to prefer a particular
    bundle over another.

    The following keys are defined by @Product@:

    BUNDLESPEC
       A bundle type specifier. These are strings passed to :prog:`bundle -t`.
       e.g. ``gzip-v2`` or ``bzip2-v1``.

    COMPRESSION
       The compression format of the bundle. e.g. ``gzip`` and ``bzip2``.

    Server operators may define custom keys.

    Example values: ``COMPRESSION=bzip2``,
    ``BUNDLESPEC=gzip-v2, COMPRESSION=gzip``.

    By default, the first bundle advertised by the server is used.

``color``
    When to colorize output. Possible value are Boolean ("yes" or "no"), or
    "debug", or "always". (default: "yes"). "yes" will use color whenever it
    seems possible. See :prog:`help color` for details.

``debug``
    Print debugging information. (default: False)

``editor``
    The editor to use during a commit. (default: ``$EDITOR`` or ``vi``)

``enableincomingoutgoing``
    Enable the commands "incoming" and "outgoing". (default: True)

``exitcodemask``
    Bitwise-and mask for the exit code of a normal command. Useful for easier
    scripting. For example, set it to 254 to normalize exit code 1 (no
    changes) to 0 (success), or set it to 63 to avoid conflicts with other
    software (ex. ``ssh``) returning 255.
    The config is effective if set via command line. If ``HGPLAIN`` is set,
    but ``HGPLAINEXCEPT`` does not contain ``exitcode``, the config is
    ineffective if set in config files.
    (default: 255)

``fallbackencoding``
    Encoding to try if it's not possible to decode the changelog using
    UTF-8. (default: ISO-8859-1)

``fancy-traceback``
    Render local variables in traceback. (default: True)

``gitignore``
    Respect ``.gitignore`` in every directory. (default: False)

``graphnodetemplate``
    The template used to print changeset nodes in an ASCII revision graph.
    (default: ``{graphnode}``)

``ignore``
    A file to read per-user ignore patterns from. This file should be
    in the same format as a repository-wide .gitignore file. Filenames
    are relative to the repository root. This option supports hook syntax,
    so if you want to specify multiple ignore files, you can do so by
    setting something like ``ignore.other = ~/.gitignore2``.

    For details of the ignore file format, see the ``gitignore(5)`` man page.

``interactive``
    Allow to prompt the user. (default: True)

``interface``
    Select the default interface for interactive features.
    Possible values are 'text', 'curses', and 'repl'. If unset, the default
    is 'repl' when Sapling is run by a coding agent, 'text' when TERM is unset
    or set to 'dumb', and 'curses' otherwise.

``interface.chunkselector``
    Select the interface for change recording (e.g. :prog:`commit -i`).
    Possible values are 'text', 'curses', and 'repl'.
    This config overrides the interface specified by ui.interface.

``logtemplate``
    Template string for commands that print changesets.

``merge``
    The conflict resolution program to use during a manual merge.
    For more information on merge tools see :prog:`help merge-tools`.
    For configuring merge tools see the ``[merge-tools]`` section.

``merge:interactive``
    If set, override ``ui.merge`` in interactive mode (usually when both
    stdin and stdout are ttys).

``mergemarkers``
    Sets the merge conflict marker label styling. The ``detailed``
    style uses the ``mergemarkertemplate`` setting to style the labels.
    The ``basic`` style just uses 'local' and 'other' as the marker label.
    One of ``basic`` or ``detailed``.
    (default: ``basic``)

``mergemarkertemplate``
    The template used to print the commit description next to each conflict
    marker during merge conflicts. See :prog:`help templates` for the template
    format.

    Defaults to showing the hash, tags, branches, bookmarks, author, and
    the first line of the commit description.

    If you use non-ASCII characters in names for tags, branches, bookmarks,
    authors, and/or commit descriptions, you must pay attention to encodings of
    managed files. At template expansion, non-ASCII characters use the encoding
    specified by the ``--encoding`` global option, ``HGENCODING`` or other
    environment variables that govern your locale. If the encoding of the merge
    markers is different from the encoding of the merged files,
    serious problems may occur.

``origbackuppath``
    The path to a directory used to store generated .orig files. If the path is
    not a directory, one will be created.  If set, files stored in this
    directory have the same name as the original file and do not have a .orig
    suffix.

``paginate``
  Control the pagination of command output (default: True). See :prog:`help pager`
  for details.

``patch``
    An optional external tool that ``@prog@ import`` and some extensions
    will use for applying patches. By default @Product@ uses an
    internal patch utility. The external tool must work as the common
    Unix ``patch`` program. In particular, it must accept a ``-p``
    argument to strip patch headers, a ``-d`` argument to specify the
    current directory, a file name to patch, and a patch file to take
    from stdin.

    It is possible to specify a patch tool together with extra
    arguments. For example, setting this option to ``patch --merge``
    will use the ``patch`` program with its 2-way merge option.

``portablefilenames``
    Check for portable filenames. Can be ``warn``, ``ignore`` or ``abort``.
    (default: ``warn``)

    ``warn``
      Print a warning message on POSIX platforms, if a file with a non-portable
      filename is added (e.g. a file with a name that can't be created on
      Windows because it contains reserved parts like ``AUX``, reserved
      characters like ``:``, or would cause a case collision with an existing
      file).

    ``ignore``
      Don't print a warning.

    ``abort``
      The command is aborted.

    ``true``
      Alias for ``warn``.

    ``false``
      Alias for ``ignore``.

    .. container:: windows

      On Windows, this configuration option is ignored and the command aborted.

``quiet``
    Reduce the amount of output printed.
    (default: False)

``remotecmd``
    Remote command to use for clone/push/pull operations.
    (default: ``hg``)

``skip-local-bookmarks-on-pull``
    Do not write local bookmarks on pull or clone.
    Turn on the ``remotenames`` extension to get remote bookmarks.

``slash``
    (Deprecated. Use ``slashpath`` template filter instead.)

    Display paths using a slash (``/``) as the path separator. This
    only makes a difference on systems where the default path
    separator is not the slash character (e.g. Windows uses the
    backslash character (``\``)).
    (default: False)

``statuscopies``
    Display copies in the status command.

``ssh``
    Command to use for SSH connections. (default: ``ssh``)

``ssherrorhint``
    A hint shown to the user in the case of SSH error (e.g.
    ``Please see http://company/internalwiki/ssh.html``)

``strict``
    Require exact command names, instead of allowing unambiguous
    abbreviations. (default: False)

``style``
    Name of style to use for command output.

``supportcontact``
    A URL where users should report a @Product@ traceback. Use this if you are a
    large organisation with its own @Product@ deployment process and crash
    reports should be addressed to your internal support.

``textwidth``
    Maximum width of help text. A longer line generated by ``@prog@ help`` or
    ``@prog@ subcommand --help`` will be broken after white space to get this
    width or the terminal width, whichever comes first.
    A non-positive value will disable this and the terminal width will be
    used. (default: 78)

``timeout``
    The timeout used when a lock is held (in seconds), a negative value
    means no timeout. (default: 600)

``timeout.warn``
    Time (in seconds) before a warning is printed about held lock. A negative
    value means no warning. (default: 0)

``traceback``
    @Product@ always prints a traceback when an unknown exception
    occurs. Setting this to True will make @Product@ print a traceback
    on all exceptions, even those recognized by @Product@ (such as
    IOError or MemoryError). (default: False)

``tweakdefaults``

    By default @Product@'s behavior changes very little from release
    to release, but over time the recommended config settings
    shift. Enable this config to opt in to get automatic tweaks to
    @Product@'s behavior over time. This config setting will have no
    effect if ``HGPLAIN` is set or ``HGPLAINEXCEPT`` is set and does
    not include ``tweakdefaults``. (default: False)

``username``
    The committer of a changeset created when running "commit".
    Typically a person's name and email address, e.g. ``Fred Widget
    <fred@example.com>``. Environment variables in the
    username are expanded.

    (default: ``username@hostname``. If the username in
    the config is empty, e.g. if the system admin set ``username =`` in the
    system config, it has to be specified manually or in a different
    config file)

``verbose``
    Increase the amount of output printed. (default: False)


``visibility``
--------------

Controls how @Product@ determines commit visibility.  @Product@
can optionally track which commits are visible explicitly, or it
can determine them implicitly from obsolescence markers.

``enabled``
    Set to true to use explicit tracking of commit visibility if
    the ``visibleheads`` requirement is set in the repo.  If False,
    or if the ``visibleheads`` requirement is not set in the repo,
    then obsolescence markers will be used to determine visibility.

``wireproto``
-------------

``logrequests``
    A list of wireproto requests to log. "sampling.py" extension can be used
    to send list of log entries to log aggregator.

``loggetfiles``
    Whether to log wireproto getfiles requests or not. "sampling.py" extension
    can be used to send list of log entries to log aggregator.

``loggetpack``
    Whether to log wireproto getpack requests or not. "sampling.py" extension
    can be used to send list of log entries to log aggregator.

Examples::

    [wireproto]
    logrequests = getbundle,gettreepack
    loggetfiles = True
    loggetpack = True

    [sampling]
    key.wireproto_requests=perfpipe_wireprotorequests

``worker``
----------

Parallel master/worker configuration. We currently perform working
directory updates in parallel on Unix-like systems, which greatly
helps performance.

``enabled``
    Whether to enable workers code to be used.
    (default: true)

``numcpus``
    Number of CPUs to use for parallel operations. A zero or
    negative value is treated as ``use the default``.
    (default: 4 or the number of CPUs on the system, whichever is larger)

``backgroundclose``
    Whether to enable closing file handles on background threads during certain
    operations. Some platforms aren't very efficient at closing file
    handles that have been written or appended to. By performing file closing
    on background threads, file write rate can increase substantially.
    (default: true on Windows, false elsewhere)

``backgroundcloseminfilecount``
    Minimum number of files required to trigger background file closing.
    Operations not writing this many files won't start background close
    threads.
    (default: 2048)

``backgroundclosemaxqueue``
    The maximum number of opened file handles waiting to be closed in the
    background. This option only has an effect if ``backgroundclose`` is
    enabled.
    (default: 384)

``backgroundclosethreadcount``
    Number of threads to process background file closes. Only relevant if
    ``backgroundclose`` is enabled.
    (default: 4)
"""


dates = r"""Some commands allow the user to specify a date, e.g.:

- backout, commit, import, tag: Specify the commit date.
- log, revert, update: Select revision(s) by date.

Many date formats are valid. Here are some examples:

- ``Wed Dec 6 13:18:29 2006`` (local timezone assumed)
- ``Dec 6 13:18 -0600`` (year assumed, time offset provided)
- ``Dec 6 13:18 UTC`` (UTC and GMT are aliases for +0000)
- ``Dec 6`` (midnight)
- ``13:18`` (today assumed)
- ``3:39`` (3:39AM assumed)
- ``3:39pm`` (15:39)
- ``2006-12-06 13:18:29`` (ISO 8601 format)
- ``2006-12-6 13:18``
- ``2006-12-6``
- ``12-6``
- ``12/6``
- ``12/6/6`` (Dec 6 2006)
- ``today`` (midnight)
- ``yesterday`` (midnight)
- ``now`` - right now

Lastly, there is @Product@'s internal format:

- ``1165411109 0`` (Wed Dec 6 13:18:29 2006 UTC)

This is the internal representation format for dates. The first number
is the number of seconds since the epoch (1970-01-01 00:00 UTC). The
second is the offset of the local timezone, in seconds west of UTC
(negative if the timezone is east of UTC).

The log command also accepts date ranges:

- ``<DATE`` - at or before a given date/time
- ``>DATE`` - on or after a given date/time
- ``DATE to DATE`` - a date range, inclusive
- ``-DAYS`` - within a given number of days of today
"""


diffs = r"""@Product@'s default format for showing changes between two versions of
a file is compatible with the unified format of GNU diff, which can be
used by GNU patch and many other standard tools.

While this standard format is often enough, it does not encode the
following information:

- executable status and other permission bits
- copy or rename information
- changes in binary files
- creation or deletion of empty files

@Product@ also supports the extended diff format from the git VCS
which addresses these limitations. The git diff format is not produced
by default because a few widespread tools still do not understand this
format.

This means that when generating diffs from a @Product@ repository
(e.g. with :prog:`export`), you should be careful about things like file
copies and renames or other things mentioned above, because when
applying a standard diff to a different repository, this extra
information is lost. @Product@'s internal operations (like push and
pull) are not affected by this, because they use an internal binary
format for communicating changes.

To make @Product@ produce the git extended diff format, use the --git
option available for many commands, or set 'git = True' in the [diff]
section of your configuration file.
"""


environment = r"""HG
    Path to the 'hg' executable, automatically passed when running
    hooks, extensions or external tools. If unset or empty, this is
    the @prog@ executable's name if it's frozen, or an executable named
    'hg' (with %PATHEXT% [defaulting to COM/EXE/BAT/CMD] extensions on
    Windows) is searched.

HGEDITOR
    This is the name of the editor to run when committing. See EDITOR.

    (deprecated, see :prog:`help config.ui.editor`)

HGENCODING
    This overrides the default locale setting detected by @Product@.
    This setting is used to convert data including usernames,
    changeset descriptions, tag names, and branches. This setting can
    be overridden with the --encoding command-line option.

HGENCODINGMODE
    This sets @Product@'s behavior for handling unknown characters
    while transcoding user input. The default is "strict", which
    causes @Product@ to abort if it can't map a character. Other
    settings include "replace", which replaces unknown characters, and
    "ignore", which drops them. This setting can be overridden with
    the --encodingmode command-line option.

HGENCODINGAMBIGUOUS
    This sets @Product@'s behavior for handling characters with
    "ambiguous" widths like accented Latin characters with East Asian
    fonts. By default, @Product@ assumes ambiguous characters are
    narrow, set this variable to "wide" if such characters cause
    formatting problems.

HGMERGE
    An executable to use for resolving merge conflicts. The program
    will be executed with three arguments: local file, remote file,
    ancestor file.

    (deprecated, see :prog:`help config.ui.merge`)

HGRCPATH
    A list of files to search for configuration files.
    Item separator is ";" or ":" on Unix, ";" on Windows. If HGRCPATH
    is not set, platform default search path is used. If empty, only
    the current repository config is read.

    Each element in HGRCPATH could start with an optional prefix
    "sys=" or "user=" to specify whether it's considered as a system
    or user config file. A special "sys=." or "user=." indicates
    reusing the existing system or user config paths. Paths without
    "sys=" or or "user=" prefixes are considered as system configs.

HGPLAIN
    When set, this disables any configuration settings that might
    change @Product@'s default output. This includes encoding,
    defaults, verbose mode, debug mode, quiet mode, tracebacks, and
    localization. This can be useful when scripting against @Product@
    in the face of existing user configuration.

    In addition to the features disabled by ``HGPLAIN=``, the following
    values can be specified to adjust behavior:

    ``+strictflags``
        Restrict parsing of command line flags.

    Equivalent options set via command line flags or environment
    variables are not overridden.

    See :prog:`help scripting` for details.

HGPLAINEXCEPT
    This is a comma-separated list of features to preserve when
    HGPLAIN is enabled. Currently the following values are supported:

    ``alias``
        Don't remove aliases.
    ``color``
        Don't disable colored output.
    ``i18n``
        Preserve internationalization.
    ``revsetalias``
        Don't remove revset aliases.
    ``templatealias``
        Don't remove template aliases.
    ``progress``
        Don't hide progress output.

    Setting HGPLAINEXCEPT to anything (even an empty string) will
    enable plain mode.

HGUSER
    This is the string used as the author of a commit. If not set,
    available values will be considered in this order:

    - HGUSER (deprecated)
    - configuration files from the HGRCPATH
    - interactive prompt
    - LOGNAME (with ``@hostname`` appended)

    (deprecated, see :prog:`help config.ui.username`)

LOGNAME
    May be used as the author of a commit; see HGUSER.

VISUAL
    This is the name of the editor to use when committing. See EDITOR.

EDITOR
    Sometimes @Product@ needs to open a text file in an editor for a
    user to modify, for example when writing commit messages. The
    editor it uses is determined by looking at the environment
    variables HGEDITOR, VISUAL and EDITOR, in that order. The first
    non-empty one is chosen. If all of them are empty, the editor
    defaults to 'vi'.

PYTHONPATH
    This is used by Python to find imported modules and may need to be
    set appropriately if this @Product@ is not installed system-wide.
"""


extensions = r"""@Product@ has the ability to add new features through the use of
extensions. Extensions may add new commands, add options to
existing commands, change the default behavior of commands, or
implement hooks.

To enable the "foo" extension, either shipped with @Product@ or in the
Python search path, create an entry for it in your configuration file,
like this::

  [extensions]
  foo =

You may also specify the full path to an extension::

  [extensions]
  myfeature = ~/.ext/myfeature.py

See :prog:`help config` for more information on configuration files.

Extensions are not loaded by default for a variety of reasons:
they can increase startup overhead; they may be meant for advanced
usage only; they may provide potentially dangerous abilities (such
as letting you destroy or modify history); they might not be ready
for prime time; or they may alter some usual behaviors of stock
@Product@. It is thus up to the user to activate extensions as
needed.

To explicitly disable an extension enabled in a configuration file of
broader scope, prepend its path with !::

  [extensions]
  # disabling extension bar residing in /path/to/extension/bar.py
  bar = !/path/to/extension/bar.py
  # ditto, but no path was supplied for extension baz
  baz = !
"""


filesets = r"""@Product@ supports a functional language for selecting a set of
files.

Like other file patterns, this pattern type is indicated by a prefix,
'set:'. The language supports a number of predicates which are joined
by infix operators. Parenthesis can be used for grouping.

Identifiers such as filenames or patterns must be quoted with single
or double quotes if they contain characters outside of
``[.*{}[]?/\_a-zA-Z0-9\x80-\xff]`` or if they match one of the
predefined predicates. This generally applies to file patterns other
than globs and arguments for predicates.

Special characters can be used in quoted identifiers by escaping them,
e.g., ``\n`` is interpreted as a newline. To prevent them from being
interpreted, strings can be prefixed with ``r``, e.g. ``r'...'``.

See also :prog:`help patterns`.

Operators
=========

There is a single prefix operator:

``not x``
  Files not in x. Short form is ``! x``.

These are the supported infix operators:

``x and y``
  The intersection of files in x and y. Short form is ``x & y``.

``x or y``
  The union of files in x and y. There are two alternative short
  forms: ``x | y`` and ``x + y``.

``x - y``
  Files in x but not in y.

Predicates
==========

The following predicates are supported:

.. predicatesmarker

Examples
========

Some sample queries:

- Show status of files that appear to be binary in the working directory::

    @prog@ status -A "set:binary()"

- Forget files that are in .gitignore but are already tracked::

    @prog@ forget "set:gitignore() and not ignored()"

- Find text files that contain a string::

    @prog@ files "set:grep(magic) and not binary()"

- Find C files in a non-standard encoding::

    @prog@ files "set:**.c and not encoding('UTF-8')"

- Revert copies of large binary files::

    @prog@ revert "set:copied() and binary() and size('>1M')"

- Revert files that were added to the working directory::

    @prog@ revert "set:revs('wdir()', added())"

- Remove files listed in foo.lst that contain the letter a or b::

    @prog@ remove "set: 'listfile:foo.lst' and (**a* or **b*)"
"""


flags = r"""Most @Product@ commands accept various flags.

Flag names
==========

Flags for each command are listed in :prog:`help` for that command.
Additionally, some flags, such as --repository, are global and can be used with
any command - those are seen in :prog:`help -v`, and can be specified before or
after the command.

Every flag has at least a long name, such as --repository. Some flags may also
have a short one-letter name, such as the equivalent -R. Using the short or long
name is equivalent and has the same effect.

Flags that have a short name can also be bundled together - for instance, to
specify both --edit (short -e) and --interactive (short -i), one could use::

    @prog@ commit -ei

If any of the bundled flags takes a value (i.e. is not a boolean), it must be
last, followed by the value::

    @prog@ commit -im 'Message'

Flag types
==========

@Product@ command-line flags can be strings, numbers, booleans, or lists of
strings.

Specifying flag values
======================

The following syntaxes are allowed, assuming a flag 'flagname' with short name
'f'::

    --flagname=foo
    --flagname foo
    -f foo
    -ffoo

This syntax applies to all non-boolean flags (strings, numbers or lists).

Specifying boolean flags
========================

Boolean flags do not take a value parameter. To specify a boolean, use the flag
name to set it to true, or the same name prefixed with 'no-' to set it to
false::

    @prog@ commit --interactive
    @prog@ commit --no-interactive

Specifying list flags
=====================

List flags take multiple values. To specify them, pass the flag multiple times::

    @prog@ files --include mercurial --include tests

Setting flag defaults
=====================

In order to set a default value for a flag in a config file, it is recommended to
use aliases::

    [alias]
    commit = commit --interactive

For more information on config files, see :prog:`help config`.

Overriding flags on the command line
====================================

If the same non-list flag is specified multiple times on the command line, the
latest specification is used::

    @prog@ commit -m "Ignored value" -m "Used value"

This includes the use of aliases - e.g., if one has::

    [alias]
    committemp = commit -m "Ignored value"

then the following command will override that -m::

    @prog@ committemp -m "Used value"

Overriding flag defaults
========================

Every flag has a default value, and you may also set your own defaults
as described above.
Except for list flags, defaults can be overridden on the command line simply by
specifying the flag in that location.

Hidden flags
============

Some flags are not shown in a command's help by default - specifically, those
that are deemed to be experimental, deprecated or advanced. To show all flags,
add the --verbose flag for the help command::

    @prog@ help --verbose commit
"""


glossary = r"""Ancestor
    Any changeset that can be reached by an unbroken chain of parent
    changesets from a given changeset. More precisely, the ancestors
    of a changeset can be defined by two properties: a parent of a
    changeset is an ancestor, and a parent of an ancestor is an
    ancestor. See also: 'Descendant'.

Bookmark
    Bookmarks are pointers to certain commits that move when
    committing. They are similar to tags in that it is possible to use
    bookmark names in all places where @Product@ expects a changeset
    ID, e.g., with :prog:`goto`. Unlike tags, bookmarks move along
    when you make a commit.

    Bookmarks can be renamed, copied and deleted. Bookmarks are local,
    unless they are explicitly pushed or pulled between repositories.
    Pushing and pulling bookmarks allow you to collaborate with others
    on a branch without creating a named branch.

Branch
    (Noun) A child changeset that has been created from a parent that
    is not a head. These are known as topological branches, see
    'Branch, topological'. If a topological branch is named, it becomes
    a named branch. If a topological branch is not named, it becomes
    an anonymous branch. See 'Branch, anonymous' and 'Branch, named'.

    Branches may be created when changes are pulled from or pushed to
    a remote repository, since new heads may be created by these
    operations. Note that the term branch can also be used informally
    to describe a development process in which certain development is
    done independently of other development. This is sometimes done
    explicitly with a named branch, but it can also be done locally,
    using bookmarks or clones and anonymous branches.

    Example: "The experimental branch."

    (Verb) The action of creating a child changeset which results in
    its parent having more than one child.

    Example: "I'm going to branch at X."

Branch, anonymous
    Every time a new child changeset is created from a parent that is not
    a head and the name of the branch is not changed, a new anonymous
    branch is created.

Branch, closed
    A named branch whose branch heads have all been closed.

Branch, default
    The branch assigned to a changeset when no name has previously been
    assigned.

Branch head
    See 'Head, branch'.

Branch, inactive
    If a named branch has no topological heads, it is considered to be
    inactive. As an example, a feature branch becomes inactive when it
    is merged into the default branch. The :prog:`branches` command
    shows inactive branches by default, though they can be hidden with
    :prog:`branches --active`.

Branch tip
    See 'Tip, branch'.

Branch, topological
    Every time a new child changeset is created from a parent that is
    not a head, a new topological branch is created. If a topological
    branch is named, it becomes a named branch. If a topological
    branch is not named, it becomes an anonymous branch of the
    current, possibly default, branch.

Changelog
    A record of the changesets in the order in which they were added
    to the repository. This includes details such as changeset id,
    author, commit message, date, and list of changed files.

Changeset
    A snapshot of the state of the repository used to record a change.

Changeset, child
    The converse of parent changeset: if P is a parent of C, then C is
    a child of P. There is no limit to the number of children that a
    changeset may have.

Changeset id
    See 'Commit hash'.

Changeset, merge
    A changeset with two parents. This occurs when a merge is
    committed.

Changeset, parent
    A revision upon which a child changeset is based. Specifically, a
    parent changeset of a changeset C is a changeset whose node
    immediately precedes C in the DAG. Changesets have at most two
    parents.

Checkout
    (Noun) The working directory being updated to a specific
    revision. This use should probably be avoided where possible, as
    changeset is much more appropriate than checkout in this context.

    Example: "I'm using checkout X."

    (Verb) Updating the working directory to a specific changeset. See
    :prog:`help update`.

    Example: "I'm going to check out changeset X."

Child changeset
    See 'Changeset, child'.

Close changeset
    See 'Head, closed branch'.

Closed branch
    See 'Branch, closed'.

Clone
    (Noun) An entire or partial copy of a repository. The partial
    clone must be in the form of a revision and its ancestors.

    Example: "Is your clone up to date?"

    (Verb) The process of creating a clone, using :prog:`clone`.

    Example: "I'm going to clone the repository."

Closed branch head
    See 'Head, closed branch'.

Commit
    (Noun) A synonym for changeset.

    Example: "Is the bug fixed in your recent commit?"

    (Verb) The act of recording changes to a repository. When files
    are committed in a working directory, @Product@ finds the
    differences between the committed files and their parent
    changeset, creating a new changeset in the repository.

    Example: "You should commit those changes now."

Commit hash
    A SHA-1 hash that uniquely identifies a changeset. It may be
    represented as either a "long" 40 hexadecimal digit string, or a
    "short" 12 hexadecimal digit string.

Cset
    A common abbreviation of the term changeset.

DAG
    The repository of changesets of a distributed version control
    system (DVCS) can be described as a directed acyclic graph (DAG),
    consisting of nodes and edges, where nodes correspond to
    changesets and edges imply a parent -> child relation. This graph
    can be visualized by graphical tools such as :prog:`log --graph`. In
    @Product@, the DAG is limited by the requirement for children to
    have at most two parents.

Deprecated
    Feature removed from documentation, but not scheduled for removal.

Default branch
    See 'Branch, default'.

Descendant
    Any changeset that can be reached by a chain of child changesets
    from a given changeset. More precisely, the descendants of a
    changeset can be defined by two properties: the child of a
    changeset is a descendant, and the child of a descendant is a
    descendant. See also: 'Ancestor'.

Diff
    (Noun) The difference between the contents and attributes of files
    in two changesets or a changeset and the current working
    directory. The difference is usually represented in a standard
    form called a "diff" or "patch". The "git diff" format is used
    when the changes include copies, renames, or changes to file
    attributes, none of which can be represented/handled by classic
    "diff" and "patch".

    Example: "Did you see my correction in the diff?"

    (Verb) Diffing two changesets is the action of creating a diff or
    patch.

    Example: "If you diff with changeset X, you will see what I mean."

Directory, working
    The working directory represents the state of the files tracked by
    @Product@, that will be recorded in the next commit. The working
    directory initially corresponds to the snapshot at an existing
    changeset, known as the parent of the working directory. See
    'Parent, working directory'. The state may be modified by changes
    to the files introduced manually or by a merge. The repository
    metadata exists in the .hg directory inside the working directory.

Draft
    Changesets in the draft phase have not been shared with publishing
    repositories and may thus be safely changed by history-modifying
    extensions. See :prog:`help phases`.

Experimental
    Feature that may change or be removed at a later date.

Graph
    See DAG and :prog:`log --graph`.

Head
    The term 'head' may be used to refer to both a branch head or a
    repository head, depending on the context. See 'Head, branch' and
    'Head, repository' for specific definitions.

    Heads are where development generally takes place and are the
    usual targets for update and merge operations.

Head, branch
    A changeset with no descendants on the same named branch.

Head, closed branch
    A changeset that marks a head as no longer interesting. The closed
    head is no longer listed by :prog:`heads`. A branch is considered
    closed when all its heads are closed and consequently is not
    listed by :prog:`branches`.

    Closed heads can be re-opened by committing new changeset as the
    child of the changeset that marks a head as closed.

Head, repository
    A topological head which has not been closed.

Head, topological
    A changeset with no children in the repository.

History, immutable
    Once committed, changesets cannot be altered.  Extensions which
    appear to change history actually create new changesets that
    replace existing ones, and then destroy the old changesets. Doing
    so in public repositories can result in old changesets being
    reintroduced to the repository.

History, rewriting
    The changesets in a repository are immutable. However, extensions
    to @Product@ can be used to alter the repository, usually in such
    a way as to preserve changeset contents.

Immutable history
    See 'History, immutable'.

Merge changeset
    See 'Changeset, merge'.

Manifest
    Each changeset has a manifest, which is the list of files that are
    tracked by the changeset.

Merge
    Used to bring together divergent branches of work. When you update
    to a changeset and then merge another changeset, you bring the
    history of the latter changeset into your working directory. Once
    conflicts are resolved (and marked), this merge may be committed
    as a merge changeset, bringing two branches together in the DAG.

Named branch
    See 'Branch, named'.

Null changeset
    The empty changeset. It is the parent state of newly-initialized
    repositories and repositories with no checked out revision. It is
    thus the parent of root changesets and the effective ancestor when
    merging unrelated changesets. Can be specified by the alias 'null'
    or by the changeset ID '000000000000'.

Parent
    See 'Changeset, parent'.

Parent changeset
    See 'Changeset, parent'.

Parent, working directory
    The working directory parent reflects a virtual revision which is
    the child of the changeset (or two changesets with an uncommitted
    merge) shown by :prog:`parents`. This is changed with
    :prog:`goto`. Other commands to see the working directory parent
    are :prog:`summary` and :prog:`id`. Can be specified by the alias ".".

Patch
    (Noun) The product of a diff operation.

    Example: "I've sent you my patch."

    (Verb) The process of using a patch file to transform one
    changeset into another.

    Example: "You will need to patch that revision."

Phase
    A per-changeset state tracking how the changeset has been or
    should be shared. See :prog:`help phases`.

Public
    Changesets in the public phase have been shared with publishing
    repositories and are therefore considered immutable. See :prog:`help
    phases`.

Pull
    An operation in which changesets in a remote repository which are
    not in the local repository are brought into the local
    repository. Note that this operation without special arguments
    only updates the repository, it does not update the files in the
    working directory. See :prog:`help pull`.

Push
    An operation in which changesets in a local repository which are
    not in a remote repository are sent to the remote repository. Note
    that this operation only adds changesets which have been committed
    locally to the remote repository. Uncommitted changes are not
    sent. See :prog:`help push`.

Repository
    The metadata describing all recorded states of a collection of
    files. Each recorded state is represented by a changeset. A
    repository is usually (but not always) found in the ``.hg``
    subdirectory of a working directory. Any recorded state can be
    recreated by "updating" a working directory to a specific
    changeset.

Repository head
    See 'Head, repository'.

Revision
    A state of the repository at some point in time. Earlier revisions
    can be updated to by using :prog:`goto`.  See also 'Revision
    number'; See also 'Changeset'.

Revision number
    Deprecated. Use 'Commit hash' instead.

    This integer uniquely identifies a changeset in a specific
    repository. It represents the order in which changesets were added
    to a repository, starting with revision number 0. Note that the
    revision number may be different in each clone of a repository. To
    identify changesets uniquely between different clones, see
    'Changeset id'.

Revlog
    History storage mechanism used by @Product@. It is a form of delta
    encoding, with occasional full revision of data followed by delta
    of each successive revision. It includes data and an index
    pointing to the data.

Rewriting history
    See 'History, rewriting'.

Root
    A changeset that has only the null changeset as its parent. Most
    repositories have only a single root changeset.

Secret
    Changesets in the secret phase may not be shared via push, pull,
    or clone. See :prog:`help phases`.

Tag
    An alternative name given to a changeset. Tags can be used in all
    places where @Product@ expects a changeset ID, e.g., with
    :prog:`goto`. The creation of a tag is stored in the history and
    will thus automatically be shared with other using push and pull.

Tip
    The changeset with the highest revision number. It is the changeset
    most recently added in a repository.

Tip, branch
    The head of a given branch with the highest revision number. When
    a branch name is used as a revision identifier, it refers to the
    branch tip. See also 'Branch, head'. Note that because revision
    numbers may be different in different repository clones, the
    branch tip may be different in different cloned repositories.

Update
    (Noun) Another synonym of changeset.

    Example: "I've pushed an update."

    (Verb) This term is usually used to describe updating the state of
    the working directory to that of a specific changeset. See
    :prog:`help update`.

    Example: "You should update."

Working directory
    See 'Directory, working'.

Working directory parent
    See 'Parent, working directory'.
"""


globals()["merge-tools"] = r"""To merge files @Product@ uses merge tools.

A merge tool combines two different versions of a file into a merged
file. Merge tools are given the two files and the greatest common
ancestor of the two file versions, so they can determine the changes
made on both branches.

Merge tools are used both for :prog:`resolve`, :prog:`merge`, :prog:`goto`,
:prog:`backout` and in several extensions.

Usually, the merge tool tries to automatically reconcile the files by
combining all non-overlapping changes that occurred separately in
the two different evolutions of the same initial base file. Furthermore, some
interactive merge programs make it easier to manually resolve
conflicting merges, either in a graphical way, or by inserting some
conflict markers. @Product@ does not include any interactive merge
programs but relies on external tools for that.

Available merge tools
=====================

External merge tools and their properties are configured in the merge-tools
configuration section - see :prog:`help config.merge-tools` - but they can often
just be named by their executable.

A merge tool is generally usable if its executable can be found on the
system and if it can handle the merge. The executable is found if it
is an absolute or relative executable path or the name of an
application in the executable search path. The tool is assumed to be
able to handle the merge if it can handle symlinks if the file is a
symlink, if it can handle binary files if the file is binary, and if a
GUI is available if the tool requires a GUI.

There are some internal merge tools which can be used. The internal
merge tools are:

.. internaltoolsmarker

Internal tools are always available and do not require a GUI but will by default
not handle symlinks or binary files.

Choosing a merge tool
=====================

@Product@ uses these rules when deciding which merge tool to use:

1. If a tool has been specified with the --tool option to merge or resolve, it
   is used.  If it is the name of a tool in the merge-tools configuration, its
   configuration is used. Otherwise the specified tool must be executable by
   the shell.

2. If the ``HGMERGE`` environment variable is present, its value is used and
   must be executable by the shell.

3. If the filename of the file to be merged matches any of the patterns in the
   merge-patterns configuration section, the first usable merge tool
   corresponding to a matching pattern is used. Here, binary capabilities of the
   merge tool are not considered.

4. If ui.merge is set it will be considered next. If the value is not the name
   of a configured tool, the specified value is used and must be executable by
   the shell. Otherwise the named tool is used if it is usable.

5. If any usable merge tools are present in the merge-tools configuration
   section, the one with the highest priority is used.

6. If a program named ``hgmerge`` can be found on the system, it is used - but
   it will by default not be used for symlinks and binary files.

7. If the file to be merged is not binary and is not a symlink, then
   internal ``:merge`` is used.

8. Otherwise, ``:prompt`` is used.

.. note::

   After selecting a merge program, @Product@ will by default attempt
   to merge the files using a simple merge algorithm first. Only if it doesn't
   succeed because of conflicting changes will @Product@ actually execute the
   merge program. Whether to use the simple merge algorithm first can be
   controlled by the premerge setting of the merge tool. Premerge is enabled by
   default unless the file is binary or a symlink.

See the merge-tools and ui sections of :prog:`help config` for details on the
configuration of merge tools.
"""


pager = r"""Some @Product@ commands can produce a lot of output, and @Product@ will
attempt to use a pager to make those commands more pleasant.

To set the pager that should be used, set the application variable::

  [pager]
  pager = less -FRX

If no pager is set in the user or repository configuration, @Product@ uses the
builtin streampager, which renders stderr and progress bars properly.

You can disable the pager for certain commands by adding them to the
pager.ignore list::

  [pager]
  ignore = version, help, update

To ignore global commands like :prog:`version` or :prog:`help`, you have
to specify them in your user configuration file.

To control whether the pager is used at all for an individual command,
you can use --pager=<value>:

  - use as needed: `auto`.
  - require the pager: `yes` or `on`.
  - suppress the pager: `no` or `off` (any unrecognized value
    will also work).

To globally turn off all attempts to use a pager, set::

  [ui]
  paginate = never

which will prevent the pager from running.

For more config options, check :prog:`help config.pager`.
"""


patterns = r"""@Product@ accepts several notations for identifying one or more files
at a time.

By default, @Product@ treats filenames as shell-style extended glob
patterns.

Alternate pattern notations must be specified explicitly.

.. note::

  Patterns specified in ``.gitignore`` are not rooted. And is different
  from patterns used by **hg** in other places.

.. note::

  In the future, patterns might be reworked to be more consistent with
  ``.gitignore``. For example, negative patterns are possible and patterns
  are orders. Things listed below might change significantly.

To use a plain path name without any pattern matching, start it with
``path:``. These path names must completely match starting at the
current repository root, and when the path points to a directory, it is matched
recursively. To match all files in a directory non-recursively (not including
any files in subdirectories), ``rootfilesin:`` can be used, specifying an
absolute path (relative to the repository root).

To use an extended glob, start a name with ``glob:``. Globs are rooted
at the current directory; a glob such as ``*.c`` will only match files
in the current directory ending with ``.c``.

The supported glob syntax extensions are ``**`` to match any string
across path separators and ``{a,b}`` to mean "a or b".

To use a Perl/Python regular expression, start a name with ``re:``.
Regexp pattern matching is anchored at the root of the repository.

To read name patterns from a file, use ``listfile:`` or ``listfile0:``.
The latter expects null delimited patterns while the former expects line
feeds. Each string read from the file is itself treated as a file
pattern.

All patterns, except for ``glob:`` specified in command line (not for
``-I`` or ``-X`` options), can match also against directories: files
under matched directories are treated as matched.
For ``-I`` and ``-X`` options, ``glob:`` will match directories recursively.

Plain examples::

  path:foo/bar        a name bar in a directory named foo in the root
                      of the repository
  path:path:name      a file or directory named "path:name"
  rootfilesin:foo/bar the files in a directory called foo/bar, but not any files
                      in its subdirectories and not a file bar in directory foo

Glob examples::

  glob:*.c       any name ending in ".c" in the current directory
  *.c            any name ending in ".c" in the current directory
  **.c           any name ending in ".c" in any subdirectory of the
                 current directory including itself.
  foo/*          any file in directory foo
  foo/**         any file in directory foo plus all its subdirectories,
                 recursively
  foo/*.c        any name ending in ".c" in the directory foo
  foo/**.c       any name ending in ".c" in any subdirectory of foo
                 including itself.

Regexp examples::

  re:.*\.c$      any name ending in ".c", anywhere in the repository

File examples::

  listfile:list.txt  read list from list.txt with one file pattern per line
  listfile0:list.txt read list from list.txt with null byte delimiters

See also :prog:`help filesets`.

Include examples::

  include:path/to/mypatternfile    reads patterns to be applied to all paths
  subinclude:path/to/subignorefile reads patterns specifically for paths in the
                                   subdirectory
"""


phases = r"""What are phases?
================

Phases are a system for tracking which changesets have been or should
be shared. This helps prevent common mistakes when modifying history.

Each changeset in a repository is in one of the following phases:

 - public : changeset is visible on a public server
 - draft : changeset is not yet published
 - secret : changeset should not be pushed, pulled, or cloned

These phases are ordered (public < draft < secret) and no changeset
can be in a lower phase than its ancestors. For instance, if a
changeset is public, all its ancestors are also public. Lastly,
changeset phases should only be changed towards the public phase.

How are phases managed?
=======================

For the most part, phases should work transparently. By default, a
changeset is created in the draft phase and is moved into the public
phase when it is pushed to another repository.

Once changesets become public, commands like amend and rebase will
refuse to operate on them to prevent creating duplicate changesets.
Phases can also be manually manipulated with the :prog:`phase` command
if needed. See :prog:`help -v phase` for examples.

To make your commits secret by default, put this in your
configuration file::

  [phases]
  new-commit = secret

Phases and servers
==================

Normally, all servers are ``publishing`` by default. This means::

 - all draft changesets that are pulled or cloned appear in phase
 public on the client

 - all draft changesets that are pushed appear as public on both
 client and server

 - secret changesets are neither pushed, pulled, or cloned

.. note::

  Pulling a draft changeset from a publishing server does not mark it
  as public on the server side due to the read-only nature of pull.

Sometimes it may be desirable to push and pull changesets in the draft
phase to share unfinished work. This can be done by setting a
repository to disable publishing in its configuration file::

  [phases]
  publish = False

See :prog:`help config` for more information on configuration files.

.. note::

  Servers running older versions of @Product@ are treated as
  publishing.

.. note::

   Changesets in secret phase are not exchanged with the server. This
   applies to their content: file names, file contents, and changeset
   metadata. For technical reasons, the identifier (e.g. d825e4025e39)
   of the secret changeset may be communicated to the server.


Examples
========

 - list changesets in draft or secret phase::

     @prog@ log -r "not public()"

 - change all secret changesets to draft::

     @prog@ phase --draft "secret()"

 - forcibly move the current changeset and descendants from public to draft::

     @prog@ phase --force --draft .

 - show a list of changeset revisions and each corresponding phase::

     @prog@ log --template "{rev} {phase}\n"

 - resynchronize draft changesets relative to a remote repository::

     @prog@ phase -fd "outgoing(URL)"

See :prog:`help phase` for more information on manually manipulating phases.
"""


revisions = r"""@Product@ supports several ways to specify revisions.

Specifying single revisions
===========================

A 40-digit hexadecimal string is treated as a unique revision identifier.
A hexadecimal string less than 40 characters long is treated as a
unique revision identifier and is referred to as a short-form
identifier. A short-form identifier is only valid if it is the prefix
of exactly one full-length identifier.

Any other string is treated as a bookmark, tag, or branch name. A
bookmark is a movable pointer to a revision. A tag is a permanent name
associated with a revision. A branch name denotes the tipmost open branch head
of that branch - or if they are all closed, the tipmost closed head of the
branch. Bookmark, tag, and branch names must not contain the ":" character.

The reserved name "tip" always identifies the most recent revision.

The reserved name "null" indicates the null revision. This is the
revision of an empty repository, and the parent of revision 0.

The reserved name "." indicates the working directory parent. If no
working directory is checked out, it is equivalent to null. If an
uncommitted merge is in progress, "." is the revision of the first
parent.

Finally, commands that expect a single revision (like ``@prog@ goto``) also
accept revsets (see below for details). When given a revset, they use the
last revision of the revset. A few commands accept two single revisions
(like ``@prog@ diff``). When given a revset, they use the first and the last
revisions of the revset.

Specifying multiple revisions
=============================

@Product@ supports a functional language for selecting a set of
revisions. Expressions in this language are called revsets.

The language supports a number of predicates which are joined by infix
operators. Parenthesis can be used for grouping.

Identifiers such as branch names may need quoting with single or
double quotes if they contain characters like ``-`` or if they match
one of the predefined predicates.

Special characters can be used in quoted identifiers by escaping them,
e.g., ``\n`` is interpreted as a newline. To prevent them from being
interpreted, strings can be prefixed with ``r``, e.g. ``r'...'``.

Operators
=========

There is a single prefix operator:

``not x``
  Changesets not in x. Short form is ``! x``.

These are the supported infix operators:

``x::y``
  A DAG range, meaning all changesets that are descendants of x and
  ancestors of y, including x and y themselves. If the first endpoint
  is left out, this is equivalent to ``ancestors(y)``, if the second
  is left out it is equivalent to ``descendants(x)``.

  An alternative syntax is ``x..y``.

``x:y``
  All changesets with revision numbers between x and y, both
  inclusive. Either endpoint can be left out, they default to 0 and
  tip.

``x and y``
  The intersection of changesets in x and y. Short form is ``x & y``.

``x or y``
  The union of changesets in x and y. There are two alternative short
  forms: ``x | y`` and ``x + y``.

``x - y``
  Changesets in x but not in y.

``x % y``
  Changesets that are ancestors of x but not ancestors of y (i.e. ::x - ::y).
  This is shorthand notation for ``only(x, y)`` (see below). The second
  argument is optional and, if left out, is equivalent to ``only(x)``.

``x^n``
  The nth parent of x, n == 0, 1, or 2.
  For n == 0, x; for n == 1, the first parent of each changeset in x;
  for n == 2, the second parent of changeset in x.

``x~n``
  The nth first ancestor of x; ``x~0`` is x; ``x~3`` is ``x^^^``.
  For n < 0, the nth unambiguous descendent of x.

``x ## y``
  Concatenate strings and identifiers into one string.

  All other prefix, infix and postfix operators have lower priority than
  ``##``. For example, ``a1 ## a2~2`` is equivalent to ``(a1 ## a2)~2``.

  For example::

    [revsetalias]
    issue(a1) = grep(r'\bissue[ :]?' ## a1 ## r'\b|\bbug\(' ## a1 ## r'\)')

  ``issue(1234)`` is equivalent to
  ``grep(r'\bissue[ :]?1234\b|\bbug\(1234\)')``
  in this case. This matches against all of "issue 1234", "issue:1234",
  "issue1234" and "bug(1234)".

There is a single postfix operator:

``x^``
  Equivalent to ``x^1``, the first parent of each changeset in x.

Patterns
========

Where noted, predicates that perform string matching can accept a pattern
string. The pattern may be either a literal, or a regular expression. If the
pattern starts with ``re:``, the remainder of the pattern is treated as a
regular expression. Otherwise, it is treated as a literal. To match a pattern
that actually starts with ``re:``, use the prefix ``literal:``.

Matching is case-sensitive, unless otherwise noted.  To perform a case-
insensitive match on a case-sensitive predicate, use a regular expression,
prefixed with ``(?i)``.

For example, ``tag(r're:(?i)release')`` matches "release" or "RELEASE"
or "Release", etc.

Predicates
==========

The following predicates are supported:

.. predicatesmarker

Performance
===========

In large repositories, certain predicates scan all commits and can be
extremely slow. Avoid these patterns:

- ``desc(pattern)`` and ``keyword(string)`` scan every commit message.
- ``all()`` selects every commit in the repository.
- ``file(pattern)`` without a revision range scans the entire history.
- ``user(pattern)`` scans all commits.

Instead, scope queries to a small set of commits (e.g. ``draft()``,
an explicit range like ``X::Y``).

Aliases
========

New predicates (known as "aliases") can be defined, using any combination of
existing predicates or other aliases. An alias definition looks like::

  <alias> = <definition>

in the ``revsetalias`` section of a @Product@ configuration file. Arguments
of the form `a1`, `a2`, etc. are substituted from the alias into the
definition.

For example,

::

  [revsetalias]
  h = heads()
  d(s) = sort(s, date)
  rs(s, k) = reverse(sort(s, k))

defines three aliases, ``h``, ``d``, and ``rs``. ``rs(0:tip, author)`` is
exactly equivalent to ``reverse(sort(0:tip, author))``.

Equivalents
===========

Command line equivalents for :prog:`log`::

  -f    ->  ::.
  -d x  ->  date(x)
  -k x  ->  keyword(x)
  -m    ->  merge()
  -u x  ->  user(x)
  -b x  ->  branch(x)
  -P x  ->  !::x
  -l x  ->  limit(expr, x)

Examples
========

Some sample queries:

- Changesets on the default branch::

    @prog@ log -r "branch(default)"

- Changesets on the default branch since tag 1.5 (excluding merges)::

    @prog@ log -r "branch(default) and 1.5:: and not merge()"

- Open branch heads::

    @prog@ log -r "head() and not closed()"

- Changesets between tags 1.3 and 1.5 mentioning "bug" that affect
  ``ext/*``::

    @prog@ log -r "1.3::1.5 and keyword(bug) and file('ext/*')"

- Changesets committed in May 2008, sorted by user::

    @prog@ log -r "sort(date('May 2008'), user)"

- Changesets mentioning "bug" or "issue" that are not in a tagged
  release::

    @prog@ log -r "(keyword(bug) or keyword(issue)) and not ancestors(tag())"

- Update to the commit that bookmark @ is pointing to, without activating the
  bookmark (this works because the last revision of the revset is used)::

    @prog@ goto :@

- Show diff between tags 1.3 and 1.5 (this works because the first and the
  last revisions of the revset are used)::

    @prog@ diff -r 1.3::1.5
"""


scripting = r"""It is common for machines (as opposed to humans) to consume @Product@.
This help topic describes some of the considerations for interfacing
machines with @Product@.

Choosing an Interface
=====================

Machines have a choice of several methods to interface with @Product@.
These include:

- Executing the ``hg`` process
- Querying a HTTP server
- Calling out to a command server

Executing ``hg`` processes is very similar to how humans interact with
@Product@ in the shell. It should already be familiar to you.

:prog:`serve` can be used to start a "command server." Clients can connect
to this server and issue @Product@ commands over a special protocol.
For more details on the command server, including links to client
libraries, see https://www.mercurial-scm.org/wiki/CommandServer.

:prog:`serve` based interfaces (the command servers) have the
advantage over simple ``hg`` process invocations in that they are
likely more efficient. This is because there is significant overhead
to spawn new Python processes.

.. tip::

   If you need to invoke several ``hg`` processes in short order and/or
   performance is important to you, use of a server-based interface
   is highly recommended.

Environment Variables
=====================

As documented in :prog:`help environment`, various environment variables
influence the operation of @Product@. The following are particularly
relevant for machines consuming @Product@:

HGPLAIN
    If not set, @Product@'s output could be influenced by configuration
    settings that impact its encoding, verbose mode, localization, etc.

    It is highly recommended for machines to set this variable when
    invoking ``hg`` processes.

HGENCODING
    If not set, the locale used by @Product@ will be detected from the
    environment. If the determined locale does not support display of
    certain characters, @Product@ may render these character sequences
    incorrectly (often by using "?" as a placeholder for invalid
    characters in the current locale).

    Explicitly setting this environment variable is a good practice to
    guarantee consistent results. "utf-8" is a good choice on UNIX-like
    environments.

HGRCPATH
    If not set, @Product@ will inherit config options from config files
    using the process described in :prog:`help config`. This includes
    inheriting user or system-wide config files.

    When utmost control over the @Product@ configuration is desired, the
    value of ``HGRCPATH`` can be set to an explicit file with known good
    configs. In rare cases, the value can be set to an empty file or the
    null device (often ``/dev/null``) to bypass loading of any user or
    system config files. Note that these approaches can have unintended
    consequences, as the user and system config files often define things
    like the username and extensions that may be required to interface
    with a repository.

Command-line Flags
==================

@Product@'s default command-line parser is designed for humans, and is not
robust against malicious input. For instance, you can start a debugger by
passing ``--debugger`` as an option value::

    $ REV=--debugger sh -c '@prog@ log -r "$REV"'

This happens because several command-line flags need to be scanned without
using a concrete command table, which may be modified while loading repository
settings and extensions.

Since @Product@ 4.4.2, the parsing of such flags may be restricted by setting
``HGPLAIN=+strictflags``. When this feature is enabled, all early options
(e.g. ``-R/--repository``, ``--cwd``, ``--config``) must be specified first
amongst the other global options, and cannot be injected to an arbitrary
location::

    $ HGPLAIN=+strictflags @prog@ -R "$REPO" log -r "$REV"

In earlier @Product@ versions where ``+strictflags`` isn't available, you
can mitigate the issue by concatenating an option value with its flag::

    $ @prog@ log -r"$REV" --keyword="$KEYWORD"

Consuming Command Output
========================

It is common for machines to need to parse the output of @Product@
commands for relevant data. This section describes the various
techniques for doing so.

Parsing Raw Command Output
--------------------------

Likely the simplest and most effective solution for consuming command
output is to simply invoke ``hg`` commands as you would as a user and
parse their output.

The output of many commands can easily be parsed with tools like
``grep``, ``sed``, and ``awk``.

A potential downside with parsing command output is that the output
of commands can change when @Product@ is upgraded. While @Product@
does generally strive for strong backwards compatibility, command
output does occasionally change. Having tests for your automated
interactions with ``hg`` commands is generally recommended, but is
even more important when raw command output parsing is involved.

Using Templates to Control Output
---------------------------------

Many ``hg`` commands support templatized output via the
``-T/--template`` argument. For more, see :prog:`help templates`.

Templates are useful for explicitly controlling output so that
you get exactly the data you want formatted how you want it. For
example, ``log -T {node}\n`` can be used to print a newline
delimited list of changeset nodes instead of a human-tailored
output containing authors, dates, descriptions, etc.

.. tip::

   If parsing raw command output is too complicated, consider
   using templates to make your life easier.

The ``-T/--template`` argument allows specifying pre-defined styles.
@Product@ ships with the machine-readable styles ``json`` and ``xml``,
which provide JSON and XML output, respectively. These are useful for
producing output that is machine readable as-is.

.. important::

   The ``json`` and ``xml`` styles are considered experimental. While
   they may be attractive to use for easily obtaining machine-readable
   output, their behavior may change in subsequent versions.

   These styles may also exhibit unexpected results when dealing with
   certain encodings. @Product@ treats things like filenames as a
   series of bytes and normalizing certain byte sequences to JSON
   or XML with certain encoding settings can lead to surprises.

Command Server Output
---------------------

If using the command server to interact with @Product@, you are likely
using an existing library/API that abstracts implementation details of
the command server. If so, this interface layer may perform parsing for
you, saving you the work of implementing it yourself.

Output Verbosity
----------------

Commands often have varying output verbosity, even when machine
readable styles are being used (e.g. ``-T json``). Adding
``-v/--verbose`` and ``--debug`` to the command's arguments can
increase the amount of data exposed by @Product@.

An alternate way to get the data you need is by explicitly specifying
a template.

Other Topics
============

revsets
   Revisions sets is a functional query language for selecting a set
   of revisions. Think of it as SQL for @Product@ repositories. Revsets
   are useful for querying repositories for specific data.

   See :prog:`help revsets` for more.

share extension
   The ``share`` extension provides functionality for sharing
   repository data across several working copies. It can even
   automatically "pool" storage for logically related repositories when
   cloning.

   Configuring the ``share`` extension can lead to significant resource
   utilization reduction, particularly around disk space and the
   network. This is especially true for continuous integration (CI)
   environments.

   See :prog:`help -e share` for more.
"""


templates = r"""@Product@ allows you to customize output of commands through
templates. You can either pass in a template or select an existing
template-style from the command line, via the --template option.

You can customize output for any "log-like" command: log,
outgoing, incoming, tip, parents, and heads.

Some built-in styles are packaged with @Product@. These can be listed
with :prog:`log --template list`. Example usage::

    $ @prog@ log -r1.0::1.1 --template changelog

A template is a piece of text, with markup to invoke variable
expansion::

    $ @prog@ log -r1 --template "{node}\n"
    b56ce7b07c52de7d5fd79fb89701ea538af65746

Keywords
========

Strings in curly braces are called keywords. The availability of
keywords depends on the exact context of the templater. These
keywords are usually available for templating a log-like command:

.. keywordsmarker

The "date" keyword does not produce human-readable output. If you
want to use a date in your output, you can use a filter to process
it. Filters are functions which return a string based on the input
variable. Be sure to use the stringify filter first when you're
applying a string-input filter to a list-like input variable.
You can also use a chain of filters to get the desired output::

   $ @prog@ tip --template "{date|isodate}\n"
   2008-08-21 18:22 +0000

Filters
========

List of filters:

.. filtersmarker

Note that a filter is nothing more than a function call, i.e.
``expr|filter`` is equivalent to ``filter(expr)``.

Functions
=========

In addition to filters, there are some basic built-in functions:

.. functionsmarker

Operators
=========

We provide a limited set of infix arithmetic operations on integers::

  + for addition
  - for subtraction
  * for multiplication
  / for floor division (division rounded to integer nearest -infinity)

Division fulfills the law x = x / y + mod(x, y).

Also, for any expression that returns a list, there is a list operator::

    expr % "{template}"

As seen in the above example, ``{template}`` is interpreted as a template.
To prevent it from being interpreted, you can use an escape character ``\{``
or a raw string prefix, ``r'...'``.

The dot operator can be used as a shorthand for accessing a sub item:

- ``expr.member`` is roughly equivalent to ``expr % '{member}'`` if ``expr``
  returns a non-list/dict. The returned value is not stringified.
- ``dict.key`` is identical to ``get(dict, 'key')``.

Aliases
========

New keywords and functions can be defined in the ``templatealias`` section of
a @Product@ configuration file::

  <alias> = <definition>

Arguments of the form `a1`, `a2`, etc. are substituted from the alias into
the definition.

For example,

::

  [templatealias]
  r = rev
  rn = "{r}:{node|short}"
  leftpad(s, w) = pad(s, w, ' ', True)

defines two symbol aliases, ``r`` and ``rn``, and a function alias
``leftpad()``.

It's also possible to specify complete template strings, using the
``templates`` section. The syntax used is the general template string syntax.

For example,

::

  [templates]
  nodedate = "{node|short}: {date(date, "%Y-%m-%d")}\n"

defines a template, ``nodedate``, which can be called like::

  $ @prog@ log -r . -Tnodedate

A template defined in ``templates`` section can also be referenced from
another template::

  $ @prog@ log -r . -T "{rev} {nodedate}"

but be aware that the keywords cannot be overridden by templates. For example,
a template defined as ``templates.rev`` cannot be referenced as ``{rev}``.

A template defined in ``templates`` section may have sub templates which
are inserted before/after/between items::

  [templates]
  myjson = ' {dict(rev, node|short)|json}'
  myjson:docheader = '\{\n'
  myjson:docfooter = '\n}\n'
  myjson:separator = ',\n'

Examples
========

Some sample command line templates:

- Format lists, e.g. files::

   $ @prog@ log -r 0 --template "files:\n{files % '  {file}\n'}"

- Join the list of files with a ", "::

   $ @prog@ log -r 0 --template "files: {join(files, ', ')}\n"

- Join the list of files ending with ".py" with a ", "::

   $ @prog@ log -r 0 --template "pythonfiles: {join(files('**.py'), ', ')}\n"

- Count the number of changed files ending with ".py"::

   $ @prog@ log -r 0 -T "{filterby('**.py', files) | count}\n"

- Separate non-empty arguments by a " "::

   $ @prog@ log -r 0 --template "{separate(' ', node, bookmarks, tags}\n"

- Modify each line of a commit description::

   $ @prog@ log --template "{splitlines(desc) % '**** {line}\n'}"

- Format date::

   $ @prog@ log -r 0 --template "{date(date, '%Y')}\n"

- Display date in UTC::

   $ @prog@ log -r 0 --template "{localdate(date, 'UTC')|date}\n"

- Output the description set to a fill-width of 30::

   $ @prog@ log -r 0 --template "{fill(desc, 30)}"

- Use a conditional to test for the default branch::

   $ @prog@ log -r 0 --template "{ifeq(branch, 'default', 'on the main branch',
   'on branch {branch}')}\n"

- Append a newline if not empty::

   $ @prog@ tip --template "{if(author, '{author}\n')}"

- Label the output for use with the color extension::

   $ @prog@ log -r 0 --template "{label('changeset.{phase}', node|short)}\n"

- Invert the firstline filter, i.e. everything but the first line::

   $ @prog@ log -r 0 --template "{sub(r'^.*\n?\n?', '', desc)}\n"

- Display the contents of the 'extra' field, one per line::

   $ @prog@ log -r 0 --template "{join(extras, '\n')}\n"

- Mark the active bookmark with '*'::

   $ @prog@ log --template "{bookmarks % '{bookmark}{ifeq(bookmark, active, '*')} '}\n"

- Find the previous release candidate tag, the distance and changes since the tag::

   $ @prog@ log -r . --template "{latesttag('re:^.*-rc$') % '{tag}, {changes}, {distance}'}\n"

- Mark the working copy parent with '@'::

   $ @prog@ log --template "{ifcontains(rev, revset('.'), '@')}\n"

- Show details of parent revisions::

   $ @prog@ log --template "{revset('parents(%d)', rev) % '{desc|firstline}\n'}"

- Show only commit descriptions that start with "template"::

   $ @prog@ log --template "{startswith('template', firstline(desc))}\n"

- Print the first word of each line of a commit message::

   $ @prog@ log --template "{word(0, desc)}\n"
"""


urls = r"""Valid URLs are of the form::

  local/filesystem/path
  file://local/filesystem/path
  https://[user[:pass]@]host[:port]/[path]
  ssh://[user@]host[:port]/[path]

Paths in the local filesystem can either point to @Product@
repositories or to bundle files (as created by :prog:`bundle` or
:prog:`incoming --bundle`). See also :prog:`help paths`.

For non-Git servers, note that HTTPS URLs depend on proper configuration
of ``web.cacerts``.

These URLs can all be stored in your configuration file with path
aliases under the ``[paths]`` section like so::

  [paths]
  alias1 = URL1
  alias2 = URL2
  ...

You can then use the alias for any command that uses a URL (for
example :prog:`pull alias1` will be treated as :prog:`pull URL1`).

There are two special aliases used as defaults when you do not provide
the URL to a command:

default:
  When you create a repository with @prog@ clone, the clone command saves
  the location of the source repository as the new repository's
  'default' path. This is then used when you omit path from push- and
  pull-like commands (including incoming and outgoing).

default-push:
  The push command will look for a path named 'default-push', and
  prefer it over 'default' if both are defined.
"""

cache = r"""@Product@ caches file contents ("data"), manifest tree entries
("manifest"), file metadata ("aux"), and LFS data ("lfs")
for shallow (i.e. lazy) repos.

Configuration
=============

The cache location is configured via ``remotefilelog.cachepath``. The cache is
namespaced by repository name allowing for a shared, system-wide cache.

Each data type ("data", "manifest", "aux", "lfs") is stored in a set of rotating
indexedlog files so as to not grow without bound. The max size and count of
log files can be configured per-type::

  [indexedlog]
  data.max-bytes-per-log = 10GB
  data.max-log-count = 4
  manifest.max-bytes-per-log = 100MB
  manifest.max-log-count = 4
  aux.max-bytes-per-log = 100MB
  aux.max-log-count = 4

  [lfs]
  blobsstoresize = 10GB
  pointersstoresize = 100MB

  # Below are legacy settings superseded by max-log-count. If max-log-count is
  # not configured, max-log-count is calculated as the below limit divided by
  # the corresponding max-bytes-per-log.
  [remotefilelog]
  cachelimit = 4GB
  manifestlimit = 1GB
  auxlimit = 1GB

As max-log-count increases, so does the potential amount of work during cache
reads.

As max-bytes-per-log increases, so does the potential for large spurts of remote
access as a cache file is deleted.

The aux cache can be disabled via the below setting::

  [scmstore]
  auxindexedlog = false

Semantics
=========

The caches are used by the scmstore file store layer. The scmstore has three
places to look for content::

  1. The cache.
  2. The local repo. File/manifest content is added to the local repo storage
     during commit operations, for example.
  3. The remote repo.

When fetching a manifest entry or file content/metadata, the logic is::

  if in local cache, return cache value
  else if in local repo, return local repo value
  else fetch remote value, write back to cache, return value


Rotation
========

When a log file crosses the size threshold, it is rotated into the next
position. Log files beyond max-log-count are deleted. Note that there are no LRU
semantics.

"""

automerge = r"""@Product@ provides a number of automatic merge algorithms to resolve
conflicts that can not be resolved in the traditional 3-way merge.

Configuration
=============

The following config sections control the behavior of automerge::

  [automerge]

  # Decision mode for the automerged result. Choose one from the list below:
  #   - prompt
  #   - accept
  #   - reject
  #   - review-in-file
  # See the `Automerge Mode` section for more information.
  mode = accept

  # A list of automerge algorithms separated by comma. Currently, we support
  # the following algorithms:
  #   - adjacent-changes
  #   - subset-changes
  #   - sort-inserts
  #   - word-merge (advanced)
  # See the `Merge Algorithms` section for more information.
  merge-algos = adjacent-changes,subset-changes

  # Disable automerge for noninteractive use cases (including ISL (VS Code), CI/CD jobs).
  # The automerge algorithms won't always give the desired result, so we disable
  # it for noninteractive cases by default.
  disable-for-noninteractive = True

Automerge Mode
==============

prompt
    Prompt the user to accept, reject the automerged result, or keep the conflict and
    automerged result in the file and review them later.

accept
    Auto accept the automerged the result.

reject
    Auto reject the automerged result. If you want to disable automerge feature, you can
    select this mode.

review-in-file
    Keep the conflict and automerged result in the file and review them later.


Merge Algorithms
================

subset-changes
    If the changes of one side is the subset of the other side, then the conflict will
    be resolved and the other side changes are kept. Currently, we limit this to additions
    on both sides.

    Here is an example of the conflict::

        <<<<<<< dest
        +
        +   if (ignore_funny && 45 < len && !memcmp(name, "refs/", 5) &&
        +       check_ref_format(name + 5))
        +     continue;
        +
        =======
        +
        +   if (ignore_funny && 45 < len && !memcmp(name, "refs/", 5) &&
        +       check_ref_format(name + 5))
        +     continue;
        +
        +   name_len = strlen(name);
        +   if (len != name_len + 41) {
        +     if (server_capabilities)
        +       free(server_capabilities);
        +     server_capabilities = strdup(name + name_len + 1);
        +   }
        +
        >>>>>>> source

    It will be resolved to::

          if (ignore_funny && 45 < len && !memcmp(name, "refs/", 5) &&
              check_ref_format(name + 5))
          continue;

          name_len = strlen(name);
          if (len != name_len + 41) {
          if (server_capabilities)
              free(server_capabilities);
          server_capabilities = strdup(name + name_len + 1);
          }

adjacent-changes
    Merge adjacent, non-overlapping modifications on both sides. The idea comes from
    BitKeeper's smerge.

    Here is an example of the conflict::

        <<<<<<< dest
          import {List} from 'immutable';
        - import {cacheMethod} from 'shared/LRU';
        + import {cached, LRU} from 'shared/LRU';
        =======
        - import {List} from 'immutable';
        + import {List, Record} from 'immutable';
          import {cacheMethod} from 'shared/LRU';
        >>>>>>> source

    It will be resolved to::

          import {List, Record} from 'immutable';
          import {cached, LRU} from 'shared/LRU';

word-merge (advanced)
    Run traditional 3-way merge on word level. This is more powerful since it's operating on
    word level instead of line level. If you want to use this algorithm, we recommend using
    it with 'prompt' mode first.

    Here is an example of the conflict::

        <<<<<<< dest
        - This is the first line.
        + That is the first line.
        =======
        - This is the first line.
        + This is a first line.
        >>>>>>> source

    It will be resolved to::

          That is a first line.

sort-inserts
    If both sides insert items in the same position of a "sorted array"
    (e.g.: import statements, buck dependencies), then union them and sort them.

    Here is an example of the conflict::

          import a
        <<<<<<< dest
        + import b
        ======= base
        + import c
        >>>>>>> source

    It will be resolved to::

          import a
          import b
          import c
"""

directorybranching = r"""@Product@ supports a form of branching reminiscent of Subversion where each branch is a separate directory.
Unlike Subversion, the branched directories live side-by-side as normal directories in the working copy.

.. note::

    Despite the "directory" nomenclature, individual files can also be branched using the same operations.


Branch Creation
===============

To branch the directory "my-project" of commit A into the directory "my-branch", use ``subtree copy``::

    $ @prog@ subtree cp -r A --from-path my-project --to-path my-branch


Grafting Between Branches (AKA Cherry Picking)
==============================================

Use ``subtree graft`` to copy a commit from one branch to another::

    $ touch my-project/file
    $ @prog@ commit -Am "add file"
    $ @prog@ subtree graft -r . --from-path my-project --to-path my-branch

Files in the grafted commit(s) outside of ``--from-path`` are ignored,
and files in the destination commit outside of ``--to-path`` are
ignored.


If you’ve already created a directory branch using the ``@prog@ cp`` or ``cp`` commands,
you can still use ``subtree graft`` to move changes between directories.

Comparing Branches
==================

The ``diff`` command supports cross-directory comparisons::

    $ echo a > my-project/file
    $ echo b > my-branch/file
    $ @prog@ subtree diff --from-path my-project --to-path my-branch

Files outside ``--from-path`` in the left side are ignored, and files
outside ``--to-path`` on the right side are ignored.


Multiple Branches
=================

Directory branching aware operations support multiple ``--from-path``/``--to-path`` mappings::

    # Create two directory branches:
    $ @prog@ subtree cp --from-path my-project --to-path my-branch1 --from-path my-project --to-path my-branch2

    $ touch my-project/file
    $ @prog@ commit -m "add file"

    # Copy working parent commit "." onto both my-branch1 and my-branch2 directories:
    $ @prog@ subtree graft -r . --from-path my-project --to-path my-branch1 --from-path my-project --to-path my-branch2

    # Modify "file" in both branches and then diff both branches against "my-project":
    $ echo branch1 > my-branch1/file
    $ echo branch2 > my-branch2/file
    $ @prog@ subtree diff --from-path my-project --to-path my-branch1 --from-path my-project --to-path my-branch2
"""
