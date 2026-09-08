# RISC-V Linux Security Internship

## 1.1.  Timeline以及回顾

### 1.1.1.   Phase1 RISC-V在linux中的基本调查

3.16:

\-    riscv存量代码行数分析

\-    riscv增量代码分析（行/周，commit/周）

\-    riscv CVE简单按照子系统分类

\-    主流内核静态动态漏洞挖掘工具的调研

3.23

\-    Riscv子系统增量趋势（commit 行数）

\-    对CVE进行分类

\-    发现用AI进行漏洞挖掘效果很好(使用gemini)，提了3个patch

3.29

\-    对CVE分类进行重构

**这个阶段做的不好的部分：**

完全没有考虑任何RISC-V的特性进行的存量/增量代码分析，比如我依然不知道Linux对于riscv的RVA23的支持是怎么样的，也不知道现在增量部分他们在给riscv加什么支持，代码行数我个人认为其实没有意义，总之这一阶段做了很多无用功，

唯一有用的就是我对于CVE的分类根据了RISC-V的子系统，但是也并没有完全分类完（只分类了19个）

这部分我认为能重新去做

### 1.1.2.   Phase2 评测其他工具在RISC-V上的能力

3.29

\-    复现Knighter工具

\-    调研isa文档到代码的中间层形式(了解到了sail)

4.5

\-    （Brainstorm）一个漏洞挖掘Layer思维模型，现依旧存在在我的思维体系中

\-    统计各个工具在riscv上的能力（smatch）

4.12

\-    统计各个工具在riscv上的能力（Knight，syzkaller）

4.19

\-    调研内核漏洞挖掘的SOTA工具

\-    初次尝试用AI复现论文

4.26

\-    对SOTA工具进行实验（Knight，bugstone，app-miner，kernelGPT）

\-    （Brainstorm）思考如何开始研究一个科研问题（问题的层次）

\-    内核中riscv代码是怎么组织的（但是这部分完全了解清楚很困难）

 

**这个阶段做的不好的部分：**

没有细致的去读任何一篇SOTA工具文章

没有细致的去研究任何一个漏洞挖掘工具（smatch， syzkaller）

AI复现论文实际价值不高，投入配置ai时间过高，但是由于不熟悉论文，就算有所产出也没有意义

### 1.1.3.   Phase3 突然出现的idea，SpecHunter

算是一个比较新颖的idea吧，但是并不是一个很有效的idea

4.30

\-    故事讲解这个idea

\-    Vibe出一个简单的工具demo，并发现四个漏洞

5.17

\-    找到12个漏洞并提交

5.24

\-    精读论文，RFDAUDIT

 

**这个阶段做的不好的部分：**

工具的完成度不是很高，特别是里面的很多组件，可能需要精读更多论文把基础组件都提升到SOTA层次，然后再加上我的那个创新点（通过Pull request定位漏洞），才能最终完成工具和论文

### 1.1.4.   Phase4 收尾

6.4

\-    帮学长看漏洞报告

6.24

\-    实习总结

## 1.2.  Deliverables

1. CVE分类
2. Patch

https://lore.kernel.org/all/20260321112827.31691-1-vulab@iscas.ac.cn/

https://lore.kernel.org/all/20260321113419.34437-1-vulab@iscas.ac.cn/

https://lore.kernel.org/all/20260322160022.21908-1-vulab@iscas.ac.cn/

https://lore.kernel.org/all/20260323115957.38348-1-vulab@iscas.ac.cn/

https://lore.kernel.org/all/20260323172826.69428-1-vulab@iscas.ac.cn/

https://lore.kernel.org/all/20260325083139.15638-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511102627.3120140-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511080904.3049446-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511072705.3015986-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511051736.2916225-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511040534.2862443-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260508174917.371667-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260511124828.3210477-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260512052240.330815-1-vulab@iscas.ac.cn/

https://patchew.org/QEMU/20260512062310.348208-1-vulab@iscas.ac.cn/

https://github.com/OpenXiangShan/ChiselIOPMP/issues/1

https://lore.kernel.org/all/20260509114122.1868327-1-vulab@iscas.ac.cn/

http://lists.infradead.org/pipermail/opensbi/2026-April/009769.html

基本全被确认了

一个linux CVE: CVE-2026-72179

 

 

## 1.3.  Future work

\-    可以重新做一下Phase1的部分，不仅于linux，还有其他的riscv生态的软件

\-    多精读论文，对可以迁移过来的论文进行复现

\-    在论文读到一定程度后全部重写SpecHunter并构思论文

## 1.4.  我学到了什么与教训

有时太注重产出了，没有细致做研究，太急了，很多部分其实都可以细化

要抓住研究的事物的特性去研究，而不是共性

 

熟悉了怎么用AI

了解了漏洞挖掘的基本方法
