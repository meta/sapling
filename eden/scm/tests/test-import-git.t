#require no-eden

  $ configure modernclient
  $ newclientrepo repo

New file:

  $ sl import -d "1000000 0" -mnew - <<EOF
  > diff --git a/new b/new
  > new file mode 100644
  > index 0000000..7898192
  > --- /dev/null
  > +++ b/new
  > @@ -0,0 +1 @@
  > +a
  > EOF
  applying patch from stdin

  $ sl tip -q
  ae3ee40d2079

New empty file:

  $ sl import -d "1000000 0" -mempty - <<EOF
  > diff --git a/empty b/empty
  > new file mode 100644
  > EOF
  applying patch from stdin

  $ sl tip -q
  ab199dc869b5

  $ sl locate empty
  empty

chmod +x:

  $ sl import -d "1000000 0" -msetx - <<EOF
  > diff --git a/new b/new
  > old mode 100644
  > new mode 100755
  > EOF
  applying patch from stdin

#if execbit
  $ sl tip -q
  3a34410f282e
  $ test -x new
#else
  $ sl tip -q
  1:ab199dc869b5
#endif

Copy and removing x bit:

  $ sl import -f -d "1000000 0" -mcopy - <<EOF
  > diff --git a/new b/copy
  > old mode 100755
  > new mode 100644
  > similarity index 100%
  > copy from new
  > copy to copy
  > diff --git a/new b/copyx
  > similarity index 100%
  > copy from new
  > copy to copyx
  > EOF
  applying patch from stdin

  $ test -f copy
#if execbit
  $ test ! -x copy
  $ test -x copyx
  $ sl tip -q
  37bacb7ca14d
#else
  $ sl tip -q
  2:0efdaa8e3bf3
#endif

  $ sl up -qCr'desc(empty)'

Copy (like above but independent of execbit):

  $ sl import -d "1000000 0" -mcopy - <<EOF
  > diff --git a/new b/copy
  > similarity index 100%
  > copy from new
  > copy to copy
  > diff --git a/new b/copyx
  > similarity index 100%
  > copy from new
  > copy to copyx
  > EOF
  applying patch from stdin

  $ sl tip -q
  0efdaa8e3bf3
  $ test -f copy

  $ cat copy
  a

  $ sl cat copy
  a

Rename:

  $ sl import -d "1000000 0" -mrename - <<EOF
  > diff --git a/copy b/rename
  > similarity index 100%
  > rename from copy
  > rename to rename
  > EOF
  applying patch from stdin

  $ sl tip -q
  b1f57753fad2

  $ sl locate
  copyx
  empty
  new
  rename

Delete:

  $ sl import -d "1000000 0" -mdelete - <<EOF
  > diff --git a/copyx b/copyx
  > deleted file mode 100755
  > index 7898192..0000000
  > --- a/copyx
  > +++ /dev/null
  > @@ -1 +0,0 @@
  > -a
  > EOF
  applying patch from stdin

  $ sl tip -q
  1bd1da94b9b2

  $ sl locate
  empty
  new
  rename

  $ test -f copyx
  [1]

Regular diff:

  $ sl import -d "1000000 0" -mregular - <<EOF
  > diff --git a/rename b/rename
  > index 7898192..72e1fe3 100644
  > --- a/rename
  > +++ b/rename
  > @@ -1 +1,5 @@
  >  a
  > +a
  > +a
  > +a
  > +a
  > EOF
  applying patch from stdin

  $ sl tip -q
  46fe99cb3035

Copy and modify:

  $ sl import -d "1000000 0" -mcopymod - <<EOF
  > diff --git a/rename b/copy2
  > similarity index 80%
  > copy from rename
  > copy to copy2
  > index 72e1fe3..b53c148 100644
  > --- a/rename
  > +++ b/copy2
  > @@ -1,5 +1,5 @@
  >  a
  >  a
  > -a
  > +b
  >  a
  >  a
  > EOF
  applying patch from stdin

  $ sl tip -q
  ffeb3197c12d

  $ sl cat copy2
  a
  a
  b
  a
  a

Rename and modify:

  $ sl import -d "1000000 0" -mrenamemod - <<EOF
  > diff --git a/copy2 b/rename2
  > similarity index 80%
  > rename from copy2
  > rename to rename2
  > index b53c148..8f81e29 100644
  > --- a/copy2
  > +++ b/rename2
  > @@ -1,5 +1,5 @@
  >  a
  >  a
  >  b
  > -a
  > +c
  >  a
  > EOF
  applying patch from stdin

  $ sl tip -q
  401aede9e6bb

  $ sl locate copy2
  [1]
  $ sl cat rename2
  a
  a
  b
  c
  a

One file renamed multiple times:

  $ sl import -d "1000000 0" -mmultirenames - <<EOF
  > diff --git a/rename2 b/rename3
  > rename from rename2
  > rename to rename3
  > diff --git a/rename2 b/rename3-2
  > rename from rename2
  > rename to rename3-2
  > EOF
  applying patch from stdin

  $ sl tip -q
  2ef727e684e8

  $ sl log -vr. --template '{files} / {file_copies}\n'
  rename2 rename3 rename3-2 / rename3 (rename2)rename3-2 (rename2)

  $ sl locate rename2 rename3 rename3-2
  rename3
  rename3-2

  $ sl cat rename3
  a
  a
  b
  c
  a

  $ sl cat rename3-2
  a
  a
  b
  c
  a

  $ echo foo > foo
  $ sl add foo
  $ sl ci -m 'add foo'

Binary files and regular patch hunks:

  $ sl import -d "1000000 0" -m binaryregular - <<EOF
  > diff --git a/binary b/binary
  > new file mode 100644
  > index 0000000000000000000000000000000000000000..593f4708db84ac8fd0f5cc47c634f38c013fe9e4
  > GIT binary patch
  > literal 4
  > Lc\${NkU|;|M00aO5
  > 
  > diff --git a/foo b/foo2
  > rename from foo
  > rename to foo2
  > EOF
  applying patch from stdin

  $ sl tip -q
  27377172366e

  $ cat foo2
  foo

  $ sl manifest --debug | grep binary
  045c85ba38952325e126c70962cc0f9d9077bc67 644   binary

Multiple binary files:

  $ sl import -d "1000000 0" -m multibinary - <<EOF
  > diff --git a/mbinary1 b/mbinary1
  > new file mode 100644
  > index 0000000000000000000000000000000000000000..593f4708db84ac8fd0f5cc47c634f38c013fe9e4
  > GIT binary patch
  > literal 4
  > Lc\${NkU|;|M00aO5
  > 
  > diff --git a/mbinary2 b/mbinary2
  > new file mode 100644
  > index 0000000000000000000000000000000000000000..112363ac1917b417ffbd7f376ca786a1e5fa7490
  > GIT binary patch
  > literal 5
  > Mc\${NkU|\`?^000jF3jhEB
  > 
  > EOF
  applying patch from stdin

  $ sl tip -q
  18b73a84b4ab

  $ sl manifest --debug | grep mbinary
  045c85ba38952325e126c70962cc0f9d9077bc67 644   mbinary1
  a874b471193996e7cb034bb301cac7bdaf3e3f46 644   mbinary2

Binary file and delta hunk (we build the patch using this sed hack to
avoid an unquoted ^, which check-code says breaks sh on Solaris):

  $ sed 's/ caret /^/g;s/ dollarparen /$(/g' > quote-hack.patch <<'EOF'
  > diff --git a/delta b/delta
  > new file mode 100644
  > index 0000000000000000000000000000000000000000..8c9b7831b231c2600843e303e66b521353a200b3
  > GIT binary patch
  > literal 3749
  > zcmV;W4qEYvP)<h;3K|Lk000e1NJLTq006iE002D*0ssI2kt{U(0000PbVXQnQ*UN;
  > zcVTj606}DLVr3vnZDD6+Qe|Oed2z{QJOBU=M@d9MRCwC#oC!>o#}>x{(W-y~UN*tK
  > z%A%sxiUy2Ys)0Vm#ueArYKoYqX;GuiqZpgirM6nCVoYk?YNAz3G~z;BZ~@~&OQEe4
  > zmGvS5isFJI;Pd_7J+EKxyHZeu` caret t4r2>F;h-+VK3{_{WoGv8dSpFDYDrA%3UX03pt
  > zOaVoi0*W#P6lDr1$`nwPDWE7*rhuYM0Y#YtiZTThWeO<D6i}2YpqR<%$s>bRRaI42
  > zS3iFIxJ8Q=EnBv1Z7?pBw_bLjJb3V+tgP(Tty_2R-mR#p04x78n2n7MSOFyt4i1iv
  > zjxH`PPEJmgD7U?IK&h;(EGQ@_DJc<@01=4fiNXHcKZ8LhZQ8T}E3U4tUS3}OrcgQW
  > zWdX{K8#l7Ev&#$ysR)G#0*rC+<WGZ3?CtG4bm-ve>Dj$|_qJ`@D*stNP_AFUe&x!Q
  > zJ9q9B7Z=ym)MyZ?Tg1ROunUYr81nV?B@!tYS~5_|%gfW#(_s<4UN1!Q?Dv8d>g#m6
  > z%*@R2@bI2JdnzxQ!EDU`$eQY!tgI~Zn$prz;gaXNod5*5p(1Bz=P$qfvZ$y?dC@X~
  > zlAD+NAKhB{=;6bMwzjqn>9mavvKOGd`s%A+fBiL>Q;xJWpa72C+}u{JTHUX>{~}Qj
  > zUb%hyHgN~c?cBLjInvUALMD9g-aXt54ZL8AOCvXL-V6!~ijR*kEG$&Mv?!pE61OlI
  > z8nzMSPE8F7bH|Py*RNl1VUCggq<V)>@_6gkEeiz7{rmTeuNTW6+KVS#0FG%IHf-3L
  > zGiS21vn>WCCr+GLx caret !uNetzB6u3o(w6&1C2?_LW8ij$+$sZ*zZ`|US3H@8N~%&V%Z
  > zAeA0HdhFS=$6|nzn3%YH`SN<>DQRO;Qc caret )dfdvA caret 5u`Xf;Zzu<ZQHgG?28V-#s<;T
  > zzkh#LA)v7gpoE5ou3o*GoUUF%b#iht&kl9d0)><$FE1}ACr68;uCA`6DrGmz_U+rp
  > zL>Rx;X_yhk$fP_yJrTCQ|NgsW0A<985g&c@k-NKly<>mgU8n||ZPPV<`SN8#%$+-T
  > zfP$T!ou8jypFVwnzqhxyUvIxXd-wF~*U!ht=hCH1wzjqn9x#)IrhDa;S0JbK caret z_$W
  > zd(8rX@;7|t*;GJ5h$SZ{v(}+UBEs$4w~?{@9%`_Z<P<kox5bMWuUWH(sF9hONgd$Q
  > zunCgwT@1|CU9+;X caret 4z&|M~@yw23Ay50NFWn=FqF%yLZEUty;AT2??1oV@B)Nt))J7
  > zh>{5j2@f7T=-an%L_`E)h;mZ4D_5>?7tjQtVPRo2XU-&;mX(!l-MSTJP4XWY82JAC
  > z@57+y&!1=P{Mn{W8)-HzEsgAtd63}Cazc>O6vGb>51%@9DzbyI3?4j~$ijmT95_IS
  > zS#r!LCDW%*4-O7CGnkr$xXR1RQ&UrA<CQt} caret 73NL%zk`)Jk!yxUAt-1r}ggLn-Zq}
  > z*s){8pw68;i+kiG%CpBKYSJLLFyq&*U8}qDp+kpe&6<Vp(Z58%l#~>ZK?&s7y?b}i
  > zuwcOgO%x-27A;y785zknl_{sU;E6v$8{pWmVS{KaJPpu`i;HP$#flY@u~Ua~K3%tN
  > z-LhrNh{9SoHgDd%WXTc$$~Dq{?AWou3!H&?V8K{ caret {P9Ot5vecD?%1&-E-ntBFj87(
  > zy5`QE%QRX7qcHC%1{Ua}M~}L6=`wQUNEQ=I;qc+ZMMXtK2T+0os;jEco;}OV9z1w3
  > zARqv caret bm-85xnRCng3OT|MyVSmR3ND7 caret ?KaQGG! caret (aTbo1N;Nz;X3Q9FJbwK6`0?Yp
  > zj*X2ac;Pw3!I2|JShDaF>-gJmzm1NLj){rk&o|$E caret WAsfrK=x&@B!`w7Hik81sPz4
  > zuJTaiCppM>-+c!wPzcUw)5@?J4U-u|pJ~xbWUe-C+60k caret 7>9!)56DbjmA~`OJJ40v
  > zu3hCA7eJXZWeN|1iJLu87$;+fS8+Kq6O`aT)*_x@sY#t7LxwoEcVw*)cWhhQW@l%!
  > z{#Z=y+qcK@%z{p*D=8_Fcg278AnH3fI5;~yGu?9TscxXaaP*4$f<LIv! caret 5Lfr%vKg
  > zpxmunH#%=+ICMvZA~wyNH%~eMl!-g caret R!cYJ#WmLq5N8viz#J%%LPtkO?V)tZ81cp>
  > z{ALK?fNPePmd;289&M8Q3>YwgZX5GcGY&n>K1<x)!`;Qjg&}bb!Lrnl@xH#kS~VYE
  > zpJmIJO`A3iy+Y3X`k>cY-@}Iw2Onq`=!ba3eATgs3yg3Wej=+P-Z8WF#w=RXvS@J3
  > zEyhVTj-gO?kfDu1g9afo<RkPrYzG#_yF41IFxF%Ylg>9lx6<clPweR-b7Hn+r)e1l
  > zO6c6FbNt@;;*w$z;N|H>h{czme)_4V6UC4hv**kX2@L caret Bgds dollarparen &P7M4dhfmWe)!=B
  > zR3X=Y{P9N}p@-##@1ZNW1YbVaiP~D@8m&<dzEP&cO|87Ju#j*=;wH~Exr>i*Hpp&@
  > z`9!Sj+O;byD~s8qZ>6QB8uv7Bpn&&?xe;;e<M4F8KEID&pT7QmqoSgq&06adp5T=U
  > z6DH*4=AB7C1D9Amu?ia-wtxSAlmTEO96XHx)-+rKP;ip$pukuSJGW3P1aUmc2yo%)
  > z&<t3F>d1X+1qzaag-%x+eKHx{?Afz3GBQSw9u0lw<mB+I#v11TKRpKWQS+lvVL7=u
  > zHr6)1ynEF<i3kO6A8&ppPMo-F=PnWfXkSj@i*7J6C<F}wR?s(O0niC?t+6;+k}pPq
  > zrok&TPU40rL0ZYDwenNrrmPZ`gjo@DEF`7 caret cKP||pUr;+r)hyn9O37=xA`3%Bj-ih
  > z+1usk<%5G-y+R?tA`qY=)6&vNjL{P?QzHg%P%>`ZxP=QB%DHY6L26?36V_p caret {}n$q
  > z3@9W=KmGI*Ng_Q#AzA%-z|Z caret |#oW(hkfgpuS$RKRhlrarX%efMMCs}GLChec5+y{6
  > z1Qnxim_C-fmQuaAK_NUHUBV&;1c0V)wji<RcdZ*aAWTwyt>hVnlt caret asFCe0&a@tqp
  > zEEy;$L}D$X6)wfQNl8gu6Z>oB3_RrP=gTyK2@@w#LbQfLNHj>Q&z(C5wUFhK+}0aV
  > zSohlc=7K+spN<ctf}5KgKqNyJDNP9;LZd)nTE=9|6Xdr9%Hzk63-tL2c9FD*rsyYY
  > z!}t+Yljq7-p$X;4_YL?6d;mdY3R##o1e%rlPxrsMh8|;sKTr~ caret QD#sw3&vS$FwlTk
  > zp1#Gw!Qo-$LtvpXt#ApV0g) caret F=qFB`VB!W297x=$mr<$>rco3v$QKih_xN!k6;M=@
  > zCr?gDNQj7tm@;JwD;Ty&NlBSCYZk(b3dZeN8D4h2{r20dSFc7;(>E&r`s=TVtzpB4
  > zk+ caret N&zCAiRns(?p6iBlk9v&h{1ve(FNtc)td51M>)TkXhc6{>5C)`fS$&)A1*CP1%
  > zld+peue4aYbg3C0!+4mu+}vE caret j_feX+ZijvffBI7Ofh#RZ*U3<3J5(+nfRCzexqQ5
  > zgM&##Y4Dd{e%ZKjqrbm@|Ni}l4jo!AqtFynj3Xsd$o caret ?yV4$|UQ(j&UWCH>M=o_&N
  > zmclXc3i|Q#<;#EoG>~V}4unTHbUK}u=y4;rA3S&vzC3 caret aJP!&D4RvvGfoyo(>C>la
  > zijP<=v>X{3Ne&2BXo}DV8l0V-jdv`$am0ubG{Wuh%CTd|l9Q7m;G&|U@#Dvbhlj(d
  > zg6W{3ATxYt#T?)3;SmIgOP4M|Dki~I_TX7SxP0x}wI~DQI7Lhm2BI7gph(aPIFAd;
  > zQ&UsF`Q{rOz+z=87c5v%@5u~d6dWV5OlX`oH3cAH&UlvsZUEo(Q(P|lKs17rXvaiU
  > zQcj}IEufi1+Bnh6&(EhF{7O3vLHp`jjlp0J<M1kh$+$2xGm~Zk7OY7(q=&Rdhq*RG
  > zwrmcd5MnP}xByB_)P@{J>DR9x6;`cUwPM8z){yooNiXPOc9_{W-gtwxE5TUg0vJk6
  > zO#JGruV&1cL6VGK2?+_YQr4`+EY8;Sm$9U$uuGRN=uj3k7?O9b+R~J7t_y*K64ZnI
  > zM+{aE<b(v?vSmw;9zFP!aE266zHIhlmdI@ caret xa6o2jwdRk54a$>pcRbC29ZyG!Cfdp
  > zutFf`Q`vljgo!(wHf=)F#m2_MIuj;L(2ja2YsQRX+rswV{d<H`Ar;(@%aNa9VPU8Z
  > z;tq*`y}dm#NDJHKlV}uTIm!_vAq5E7!X-p{P=Z=Sh668>PuVS1*6e}OwOiMc;u3OQ
  > z@Bs)w3=lzfKoufH$SFuPG@uZ4NOnM#+=8LnQ2Q4zUd+nM+OT26;lqbN{P07dhH{jH
  > zManE8 caret dLms-Q2;1kB<*Q1a3f8kZr;xX=!Qro@`~@xN*Qj>gx;i;0Z24!~i2uLb`}v
  > zA?R$|wvC+m caret Ups=*(4lDh*=UN8{5h(A?p#D caret 2N$8u4Z55!q?ZAh(iEEng9_Zi>IgO
  > z#~**JC8hE4@n{hO&8btT5F*?nC_%LhA3i)PDhh-pB_&1wGrDIl caret *=8x3n&;akBf caret -
  > zJd&86kq$%%907v caret tgWoQdwI`|oNK%VvU~S#C<o caret F?6c48?Cjj#-4P<>HFD%&|Ni~t
  > zKJ(|#H`$<5W+6ZkBb213rXonKZLB+X> caret L}J@W6osP3piLD_5?R!`S}*{xLBzFiL4@
  > zX+}l{`A%?f@T5tT%ztu60p;)be`fWC`tP@WpO=?cpf8Xuf1OSj6d3f@Ki(ovDYq%0
  > z{4ZSe`kOay5@=lAT!}vFzxyemC{sXDrhuYM0Y#ZI1r%ipD9W11{w=@&xgJ}t2x;ep
  > P00000NkvXXu0mjfZ5|Er
  > 
  > literal 0
  > HcmV?d00001
  > 
  > EOF
  $ sl import -d "1000000 0" -m delta quote-hack.patch
  applying quote-hack.patch
  $ rm quote-hack.patch

  $ sl manifest --debug | grep delta
  9600f98bb60ce732634d126aaa4ac1ec959c573e 644   delta

  $ sl import -d "1000000 0" -m delta - <<'EOF'
  > diff --git a/delta b/delta
  > index 8c9b7831b231c2600843e303e66b521353a200b3..0021dd95bc0dba53c39ce81377126d43731d68df 100644
  > GIT binary patch
  > delta 49
  > zcmZ1~yHs|=21Z8J$r~9bFdA-lVv=EEw4WT$qRf2QSa5SIOAHI6(&k4T8H|kLo4vWB
  > FSO9ZT4bA`n
  > 
  > delta 49
  > zcmV-10M7rV9i<(xumJ(}ld%Di0Xefm0vrMXpOaq%BLm9I%d>?9Tm%6Vv*HM70RcC&
  > HOA1;9yU-AD
  > 
  > EOF
  applying patch from stdin

  $ sl manifest --debug | grep delta
  56094bbea136dcf8dbd4088f6af469bde1a98b75 644   delta

Filenames with spaces:

  $ sed 's,EOL$,,g' <<EOF | sl import -d "1000000 0" -m spaces -
  > diff --git a/foo bar b/foo bar
  > new file mode 100644
  > index 0000000..257cc56
  > --- /dev/null
  > +++ b/foo bar	EOL
  > @@ -0,0 +1 @@
  > +foo
  > EOF
  applying patch from stdin

  $ sl tip -q
  4b79479c9a6d

  $ cat "foo bar"
  foo

Copy then modify the original file:

  $ sl import -d "1000000 0" -m copy-mod-orig - <<EOF
  > diff --git a/foo2 b/foo2
  > index 257cc56..fe08ec6 100644
  > --- a/foo2
  > +++ b/foo2
  > @@ -1 +1,2 @@
  >  foo
  > +new line
  > diff --git a/foo2 b/foo3
  > similarity index 100%
  > copy from foo2
  > copy to foo3
  > EOF
  applying patch from stdin

  $ sl tip -q
  9cbe44af4ae9

  $ cat foo3
  foo

Move text file and patch as binary

  $ echo a > text2
  $ sl ci -Am0
  adding text2
  $ sl import -d "1000000 0" -m rename-as-binary - <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > literal 5
  > Mc$`b*O5$Pw00T?_*Z=?k
  > 
  > EOF
  applying patch from stdin

  $ cat binary2
  a
  b
  \x00 (no-eol) (esc)

  $ sl st --copies --change .
  A binary2
    text2
  R text2

Invalid base85 content

  $ sl revert -aq
  $ sl import -d "1000000 0" -m invalid-binary - <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > literal 5
  > Mc$`b*O.$Pw00T?_*Z=?k
  > 
  > EOF
  applying patch from stdin
  abort: could not decode "binary2" binary patch: bad base85 character at position 6
  [255]

  $ sl revert -aq
  $ sl import -d "1000000 0" -m rename-as-binary - <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > literal 6
  > Mc$`b*O5$Pw00T?_*Z=?k
  > 
  > EOF
  applying patch from stdin
  abort: "binary2" length is 5 bytes, should be 6
  [255]

  $ sl revert -aq
  $ sl import -d "1000000 0" -m rename-as-binary - <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > Mc$`b*O5$Pw00T?_*Z=?k
  > 
  > EOF
  applying patch from stdin
  abort: could not extract "binary2" binary data
  [255]

Truncated zlib stream (the data line is cut short)

  $ sl revert -aq
  $ sl import -d "1000000 0" -m truncated-binary - <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > literal 5
  > Mc$`b*O5$Pw
  > 
  > EOF
  applying patch from stdin
  abort: could not decode "binary2" binary patch: Error -5 while decompressing data: incomplete or truncated stream
  [255]

Simulate a copy/paste turning LF into CRLF (issue2870)

  $ sl revert -aq
  $ cat > binary.diff <<"EOF"
  > diff --git a/text2 b/binary2
  > rename from text2
  > rename to binary2
  > index 78981922613b2afb6025042ff6bd878ac1994e85..10efcb362e9f3b3420fcfbfc0e37f3dc16e29757
  > GIT binary patch
  > literal 5
  > Mc$`b*O5$Pw00T?_*Z=?k
  > 
  > EOF
  >>> fp = open('binary.diff', 'rb')
  >>> data = fp.read()
  >>> fp.close()
  >>> _ = open('binary.diff', 'wb').write(data.replace(b'\n', b'\r\n'))
  $ sl mv binary2 text2
  $ sl commit -m foo
  $ sl import --no-commit binary.diff
  applying binary.diff

  $ cd ..

Consecutive import with renames (issue2459)

  $ newclientrepo issue2459
  $ sl import --no-commit --force - <<EOF
  > diff --git a/a b/a
  > new file mode 100644
  > EOF
  applying patch from stdin
  $ sl import --no-commit --force - <<EOF
  > diff --git a/a b/b
  > rename from a
  > rename to b
  > EOF
  applying patch from stdin
  a has not been committed yet, so no copy data will be stored for b.
  $ sl debugstate
  a   0         -1 unset               b
  $ sl ci -m done
  $ cd ..

Renames and strip

  $ newclientrepo renameandstrip
  $ echo a > a
  $ sl ci -Am adda
  adding a
  $ sl import --no-commit -p2 - <<EOF
  > diff --git a/foo/a b/foo/b
  > rename from foo/a
  > rename to foo/b
  > EOF
  applying patch from stdin
  $ sl st --copies
  A b
    a
  R a

Prefix with strip, renames, creates etc

  $ sl revert -aC
  undeleting a
  forgetting b
  $ rm b
  $ mkdir -p dir/dir2
  $ echo b > dir/dir2/b
  $ echo c > dir/dir2/c
  $ echo d > dir/d
  $ sl ci -Am addbcd
  adding dir/d
  adding dir/dir2/b
  adding dir/dir2/c

prefix '.' is the same as no prefix
  $ sl import --no-commit --prefix . - <<EOF
  > diff --git a/dir/a b/dir/a
  > --- /dev/null
  > +++ b/dir/a
  > @@ -0,0 +1 @@
  > +aaaa
  > diff --git a/dir/d b/dir/d
  > --- a/dir/d
  > +++ b/dir/d
  > @@ -1,1 +1,2 @@
  >  d
  > +dddd
  > EOF
  applying patch from stdin
  $ cat dir/a
  aaaa
  $ cat dir/d
  d
  dddd
  $ sl revert -aC
  forgetting dir/a
  reverting dir/d
  $ rm dir/a

prefix with default strip
  $ sl import --no-commit --prefix dir/ - <<EOF
  > diff --git a/a b/a
  > --- /dev/null
  > +++ b/a
  > @@ -0,0 +1 @@
  > +aaa
  > diff --git a/d b/d
  > --- a/d
  > +++ b/d
  > @@ -1,1 +1,2 @@
  >  d
  > +dd
  > EOF
  applying patch from stdin
  $ cat dir/a
  aaa
  $ cat dir/d
  d
  dd
  $ sl revert -aC
  forgetting dir/a
  reverting dir/d
  $ rm dir/a
(test that prefixes are relative to the cwd)
  $ mkdir tmpdir
  $ cd tmpdir
  $ sl import --no-commit -p2 --prefix ../dir/ - <<EOF
  > diff --git a/foo/a b/foo/a
  > new file mode 100644
  > --- /dev/null
  > +++ b/foo/a
  > @@ -0,0 +1 @@
  > +a
  > diff --git a/foo/dir2/b b/foo/dir2/b2
  > rename from foo/dir2/b
  > rename to foo/dir2/b2
  > diff --git a/foo/dir2/c b/foo/dir2/c
  > --- a/foo/dir2/c
  > +++ b/foo/dir2/c
  > @@ -0,0 +1 @@
  > +cc
  > diff --git a/foo/d b/foo/d
  > deleted file mode 100644
  > --- a/foo/d
  > +++ /dev/null
  > @@ -1,1 +0,0 @@
  > -d
  > EOF
  applying patch from stdin
  $ sl st --copies
  M ../dir/dir2/c
  A ../dir/a
  A ../dir/dir2/b2
    ../dir/dir2/b
  R ../dir/d
  R ../dir/dir2/b
  $ cd ..

Renames, similarity and git diff

  $ sl revert -aC
  forgetting dir/a
  undeleting dir/d
  undeleting dir/dir2/b
  forgetting dir/dir2/b2
  reverting dir/dir2/c
  $ rm dir/a dir/dir2/b2
  $ sl import --similarity 90 --no-commit - <<EOF
  > diff --git a/a b/b
  > rename from a
  > rename to b
  > EOF
  applying patch from stdin
  $ sl st --copies
  A b
    a
  R a
  $ cd ..

Pure copy with existing destination

  $ newclientrepo copytoexisting
  $ echo a > a
  $ echo b > b
  $ sl ci -Am add
  adding a
  adding b
  $ sl import --no-commit - <<EOF
  > diff --git a/a b/b
  > copy from a
  > copy to b
  > EOF
  applying patch from stdin
  abort: cannot create b: destination already exists
  [255]
  $ cat b
  b

Copy and changes with existing destination

  $ sl import --no-commit - <<EOF
  > diff --git a/a b/b
  > copy from a
  > copy to b
  > --- a/a
  > +++ b/b
  > @@ -1,1 +1,2 @@
  > a
  > +b
  > EOF
  applying patch from stdin
  cannot create b: destination already exists
  1 out of 1 hunks FAILED -- saving rejects to file b.rej
  abort: patch failed to apply
  [255]
  $ cat b
  b

#if symlink

  $ ln -s b linkb
  $ sl add linkb
  $ sl ci -m addlinkb
  $ sl import --no-commit - <<EOF
  > diff --git a/linkb b/linkb
  > deleted file mode 120000
  > --- a/linkb
  > +++ /dev/null
  > @@ -1,1 +0,0 @@
  > -badhunk
  > \ No newline at end of file
  > EOF
  applying patch from stdin
  patching file linkb
  Hunk #1 FAILED at 0
  1 out of 1 hunks FAILED -- saving rejects to file linkb.rej
  abort: patch failed to apply
  [255]
  $ sl st
  ? b.rej
  ? linkb.rej

#endif

Test corner case involving copies and multiple hunks (issue3384)

  $ sl revert -qa
  $ sl import --no-commit - <<EOF
  > diff --git a/a b/c
  > copy from a
  > copy to c
  > --- a/a
  > +++ b/c
  > @@ -1,1 +1,2 @@
  >  a
  > +a
  > @@ -2,1 +2,2 @@
  >  a
  > +a
  > diff --git a/a b/a
  > --- a/a
  > +++ b/a
  > @@ -1,1 +1,2 @@
  >  a
  > +b
  > EOF
  applying patch from stdin

Test email metadata

  $ sl revert -qa
  $ sl --encoding utf-8 import - <<EOF
  > From: =?UTF-8?q?Rapha=C3=ABl=20Hertzog?= <hertzog@debian.org>
  > Subject: [PATCH] =?UTF-8?q?=C5=A7=E2=82=AC=C3=9F=E1=B9=AA?=
  > 
  > diff --git a/a b/a
  > --- a/a
  > +++ b/a
  > @@ -1,1 +1,2 @@
  >  a
  > +a
  > EOF
  applying patch from stdin
  $ sl --encoding utf-8 log -r .
  commit:      * (glob)
  user:        Raphaël Hertzog <hertzog@debian.org>
  date:        * (glob)
  summary:     \xc5\xa7\xe2\x82\xac\xc3\x9f\xe1\xb9\xaa (esc)
  
  $ cd ..
