# Overview

## Interaction between minecraft and attendants

```text
┌─────────┐      filter               ┌─────────┐    _______
│mc_output│==========================>│client_io│---/socket/
└───\─────┘ :broadcast<FilterOutput>  └─────────┘   ‾‾‾‾‾‾‾
   __\_________________                  ¦  ʌ
  /SharedOutputFilters/               ╲¦ ¦  ‖ reply
  ‾‾‾/‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾‾                 ╲ v  ‖ :mpsc<AttendantAnswer>
┌───/─────┐ req :mpsc<ClientRequest>  ┌──────────────┐
│mc_output│<==========================│client_process│
│         │==========================>│              │
└───\─────┘   req.done :ready         └──────────────┘
 ____\________
/InputFilters/
‾‾‾‾‾‾‾‾‾‾‾‾‾
```   
