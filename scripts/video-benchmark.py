#!/usr/bin/env python3
"""Run deterministic matched desktop encoder cases and decoded image comparisons.

Requires compiler, FFmpeg development libs, ffmpeg and python-numpy. This measures
native conversion/encoding plus synthetic decoded quality; it never claims
capture, transport, application, client, or physical presentation performance.
"""
import argparse
import itertools
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
PROFILES = {"speed": (1, 0), "balanced": (4, 1), "quality": (5, 1)}
WORKLOADS = ("idle", "typing", "scroll", "color-text", "move", "video", "resize", "refinement")


def command(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs)


def compare(stream, width, height, avc444, save_images, last_frame=199, suffix=""):
    import numpy as np
    cw, ch = (width + 15) & ~15, (height + 15) & ~15
    wanted = f"eq(n,{last_frame*2})+eq(n,{last_frame*2+1})" if avc444 else f"eq(n,{last_frame})"
    raw = command(["ffmpeg", "-v", "error", "-threads", "1", "-i", str(stream),
                   "-vf", f"select='{wanted}'", "-fps_mode", "passthrough", "-pix_fmt", "yuvj420p",
                   "-f", "rawvideo", "-threads", "1", "pipe:1"]).stdout
    frame_size = cw * ch * 3 // 2
    if len(raw) != frame_size * (2 if avc444 else 1):
        raise RuntimeError(f"unexpected decoded shape: {len(raw)} bytes")

    def planes(data):
        array = np.frombuffer(data, dtype=np.uint8).astype(np.float32)
        n = cw * ch
        return array[:n].reshape(ch, cw), array[n:n+n//4].reshape(ch//2,cw//2), array[n+n//4:].reshape(ch//2,cw//2)

    y, main_u, main_v = planes(raw[:frame_size])
    u, v = np.repeat(np.repeat(main_u, 2, axis=0), 2, axis=1), np.repeat(np.repeat(main_v, 2, axis=0), 2, axis=1)
    if avc444:
        aux_y, aux_u, aux_v = planes(raw[frame_size:])
        blocks = aux_y.reshape(ch//16,16,cw)
        odd_u, odd_v = blocks[:,:8,:].reshape(ch//2,cw), blocks[:,8:,:].reshape(ch//2,cw)
        u[1::2,:], v[1::2,:] = odd_u, odd_v
        u[::2,1::2], v[::2,1::2] = aux_u, aux_v
        # Optional RDP reverse filter, threshold30, MS-RDPEGFX3.3.8.3.2.
        for full, main, odd, aux in ((u,main_u,odd_u,aux_u),(v,main_v,odd_v,aux_v)):
            recovered = np.clip(4*main - aux - odd[:,::2] - odd[:,1::2],0,255)
            full[::2,::2] = np.where(np.abs(recovered-main)>30,recovered,main)
    y, u, v = y[:height,:width], u[:height,:width]-128, v[:height,:width]-128
    rgb = np.stack((y+1.5748*v, y-.187324*u-.468124*v, y+1.8556*u),axis=-1)
    rgb = np.clip(rgb,0,255)
    reference = np.fromfile(str(stream)+".reference.bgra",dtype=np.uint8).reshape(height,width,4)[:,:,[2,1,0]].astype(np.float32)
    difference = rgb-reference
    mse = float(np.mean(difference*difference))
    result = {"rgb_mse":mse,"rgb_psnr_db":10*math.log10(255*255/mse) if mse else None,
              "rgb_mean_absolute_error":float(np.mean(np.abs(difference))),"rgb_max_absolute_error":float(np.max(np.abs(difference)))}
    if save_images:
        for label, image in (("decoded",rgb),("reference",reference)):
            command(["ffmpeg","-v","error","-f","rawvideo","-pix_fmt","rgb24","-s",f"{width}x{height}","-i","pipe:0","-frames:v","1","-y",str(stream)+f"{suffix}.{label}.png"],input=np.rint(image).astype(np.uint8).tobytes())
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--software",action="store_true")
    parser.add_argument("--sizes",nargs="+",default=["1920x1080","3840x2160"])
    parser.add_argument("--profiles",nargs="+",choices=PROFILES,default=["speed"])
    parser.add_argument("--workloads",nargs="+",choices=WORKLOADS,default=list(WORKLOADS))
    parser.add_argument("--bitrates",nargs="+",type=int,default=[12000000])
    parser.add_argument("--codecs",nargs="+",choices=["avc420","avc444"],default=["avc420"])
    parser.add_argument("--repeat",type=int,default=2)
    parser.add_argument("--save-images",action="store_true")
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    flags=command(["pkg-config","--cflags","--libs","libavcodec","libavutil","libswscale"],text=True).stdout
    executable=args.output.resolve()/"encoder-bench"
    command(["cc","-O3","-Wall","-Wextra",str(ROOT/"scripts/encoder-bench.c"),"-o",str(executable),*shlex.split(flags)])
    gpu = subprocess.run(["nvidia-smi","--query-gpu=name,driver_version,pci.bus_id","--format=csv,noheader"],capture_output=True,text=True) if not args.software else None
    metadata={"scope":"synthetic_native_encoder","physical_presentation_measured":False,"date_utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),
              "machine":platform.platform(),"revision":command(["git","rev-parse","HEAD"],cwd=ROOT,text=True).stdout.strip(),
              "working_tree_modified":bool(command(["git","status","--porcelain"],cwd=ROOT,text=True).stdout),
              "gpu":gpu.stdout.strip() if gpu else None,"arguments":vars(args)|{"output":str(args.output)}}
    metadata["source_sha256"]={str(path.relative_to(ROOT)):hashlib.sha256(path.read_bytes()).hexdigest() for path in (ROOT/"bridge/src/video.c",ROOT/"bridge/src/video_gpu.c",ROOT/"scripts/encoder-bench.c")}
    (args.output/"metadata.json").write_text(json.dumps(metadata,indent=2)+"\n")
    with (args.output/"results.jsonl").open("w") as report:
        for repeat,size,workload,bitrate,codec,profile in itertools.product(range(args.repeat),args.sizes,args.workloads,args.bitrates,args.codecs,args.profiles):
            w,h=map(int,size.split("x"));preset,aq=PROFILES[profile]
            label=f"r{repeat}-{size}-{workload}-{bitrate}-{codec}-{profile}"
            stream=args.output/f"{label}.h264"
            result=command([str(executable),str(int(args.software)),str(w),str(h),"60",str(bitrate),str(preset),str(aq),workload,str(int(codec=="avc444")),str(stream)])
            (args.output/f"{label}.stderr").write_bytes(result.stderr)
            receipt=json.loads(result.stdout);receipt.update({"repeat":repeat,"profile":profile,"synthetic_quality":True})
            # Resize changes sequence dimensions and has a separate integration
            # contract. Avoid comparing its final coded image at the initial size.
            if workload!="resize":receipt["decoded_quality"]=compare(stream,w,h,codec=="avc444",args.save_images)
            if workload=="refinement":receipt["before_refinement_quality"]=compare(stream,w,h,codec=="avc444",args.save_images,last_frame=198,suffix=".before-refinement")
            report.write(json.dumps(receipt)+"\n");report.flush()
            print(json.dumps(receipt),flush=True)
    return 0


if __name__=="__main__":
    raise SystemExit(main())
