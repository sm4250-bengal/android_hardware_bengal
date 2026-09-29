# Bengal Vulkan multiview smoke

This is an inert vendor binary source.  No product, init service, or probe
currently installs it.  It intentionally does not alter the existing
offscreen or WSI smoke contracts.

The smoke requires a physical Vulkan 1.3-or-newer Qualcomm Adreno 610 device,
Mesa Turnip driver identity, and the `multiview` feature.  It renders two
array-image layers in one subpass with `viewMask = 0x3`; both vertex and
fragment shaders consume `gl_ViewIndex`.  Layer zero has an interior red
triangle on black and layer one has a displaced green triangle on black.  It
copies each layer separately to host-visible memory and verifies interior and
outside-clear pixels after a transfer-to-host barrier. It also checks that each
triangle is absent from the opposite layer, rejecting a broadcast-to-both bug.
The client requests Vulkan1.3, requires at least two multiview views, bounds
enumerations and allocations (16MiB each), and retains Vulkan objects on any
unproved submission completion. An external wall-clock timeout is still needed
around driver calls other than the five-second fence wait.

`--allow-software-control` is deliberately a non-hardware control: a
successful Vulkan1.3-or-newer CPU rendering run exits3 and prints `NOT_HARDWARE`,
never a device result. This mode rejects hardware GPUs. The normal invocation
rejects every non-exact identity.

Reproducible shader identities:

* `multiview.vert`: `60b7e4e3191cdb6ebc604668ec7ec946e51631a1dcbc83bc0209fd7809ff9894`; SPIR-V 1,676 bytes, `11ce94bdfe01c7b9948242cec61a23dab69d9b0acc3ec35d9e08ebc6760a2a24`.
* `multiview.frag`: `97d0c1175d9561c919539a4e199739c91fd225935aad40775d815f37f7887c2b`; SPIR-V 972 bytes, `89198093b27d320e61e48c50c2c0e2c5ef1f28230ad0bdf57e6dad6d403fd3b1`.

Use `tools/verify-shaders.sh`, then `tools/build.sh host` or `android` with
absolute `TMPDIR`/`BENGAL_DESK_TMP`.  The host regression is explicitly
forced to one CPU ICD through `BENGAL_SOFTWARE_VULKAN_ICD`; it never selects a
host GPU.
